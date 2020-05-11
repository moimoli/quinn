use std::{
    error::Error as StdError,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{ready, Stream as _};
use http_body::Body as HttpBody;
use pin_project::{pin_project, project};
use quinn::SendStream;
use quinn_proto::StreamId;

use crate::{
    body::RecvBody,
    connection::ConnectionRef,
    frame::{FrameStream, WriteFrame},
    headers::{DecodeHeaders, SendHeaders},
    proto::{
        frame::{DataFrame, HttpFrame},
        headers::Header,
        ErrorCode,
    },
    streams::Reset,
    Error, HttpError,
};

/// Represent data transmission completion for a Request or a Response
///
/// This is yielded by [`SendRequest`] and [`SendResponse`]. It will encode and send
/// the headers, then send the body if any data is polled from [`HttpBody::poll_data()`].
/// It also encodes and sends the trailer a similar way, if any.
#[pin_project]
pub struct SendData<B, P> {
    headers: Option<Header>,
    #[pin]
    body: B,
    #[pin]
    state: SendDataState<P>,
    conn: ConnectionRef,
    send: Option<SendStream>,
    stream_id: StreamId,
    finish: bool,
}

#[pin_project]
enum SendDataState<P> {
    Initial,
    Headers(SendHeaders),
    PollBody,
    Write(#[pin] WriteFrame<DataFrame<P>>),
    PollTrailers,
    Trailers(SendHeaders),
    Closing,
    Finished,
}

impl<B> SendData<B, B::Data>
where
    B: HttpBody + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>> + Send + Sync,
{
    pub(crate) fn new(
        send: SendStream,
        conn: ConnectionRef,
        headers: Header,
        body: B,
        finish: bool,
    ) -> Self {
        Self {
            conn,
            body,
            finish,
            headers: Some(headers),
            stream_id: send.id(),
            send: Some(send),
            state: SendDataState::Initial,
        }
    }

    /// Cancel the request
    ///
    /// The peer will receive a request error with `REQUEST_CANCELLED` code.
    pub fn cancel(&mut self) {
        self.state = SendDataState::Finished;
        match self.state {
            SendDataState::Write(ref mut w) => {
                w.reset(ErrorCode::REQUEST_CANCELLED);
            }
            SendDataState::Trailers(ref mut w) => {
                w.reset(ErrorCode::REQUEST_CANCELLED);
            }
            _ => {
                if let Some(ref mut send) = self.send.take() {
                    send.reset(ErrorCode::REQUEST_CANCELLED.into());
                }
            }
        }
        self.state = SendDataState::Finished;
    }

    /// Monitor stop sending signal from the peer
    ///
    /// This will return `Ready` when a STOP_SENDING frame from the peer has
    /// been received for this stream. Else, it will return `Pending` indefinitely.
    ///
    /// When stopped, the reason of the peer's closure will be returned. Else,
    /// `None` will be returned, signifying the stream closure has been
    /// triggered from another event.
    pub fn poll_stopped(&mut self, cx: &mut Context) -> Poll<Option<HttpError>> {
        let reason = match self.state {
            SendDataState::Write(ref mut w) => ready!(w.poll_stopped(cx)),
            SendDataState::Trailers(ref mut w) => ready!(w.poll_stopped(cx)),
            _ => match self.send {
                None => return Poll::Ready(None),
                Some(ref mut send) => ready!(send.poll_stopped(cx)).map(Into::into),
            },
        };
        Poll::Ready(reason.map(Into::into))
    }
}

impl<B> Future for SendData<B, B::Data>
where
    B: HttpBody + 'static,
    B::Error: Into<Box<dyn StdError + Send + Sync>> + Send + Sync,
{
    type Output = Result<(), Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut me = self.project();
        loop {
            #[project]
            match &mut me.state.as_mut().project() {
                SendDataState::Initial => {
                    // This initial computaion is done here to report its failability to Future::Output.
                    let header = me.headers.take().expect("headers");
                    me.state.set(SendDataState::Headers(SendHeaders::new(
                        header,
                        &me.conn,
                        me.send.take().expect("send"),
                        *me.stream_id,
                    )?));
                }
                SendDataState::Headers(ref mut send) => {
                    *me.send = Some(ready!(Pin::new(send).poll(cx))?);
                    me.state.set(SendDataState::PollBody);
                }
                SendDataState::PollBody => {
                    let next = match ready!(Pin::new(&mut me.body).poll_data(cx)) {
                        None => SendDataState::PollTrailers,
                        Some(Err(e)) => return Poll::Ready(Err(Error::body(e.into()))),
                        Some(Ok(d)) => {
                            let send = me.send.take().expect("send");
                            let data = DataFrame { payload: d };
                            SendDataState::Write(WriteFrame::new(send, data))
                        }
                    };
                    me.state.set(next);
                }
                SendDataState::Write(ref mut write) => {
                    *me.send = Some(ready!(Pin::new(write).poll(cx))?);
                    me.state.set(SendDataState::PollBody);
                }
                SendDataState::PollTrailers => {
                    match ready!(Pin::new(&mut me.body).poll_trailers(cx))
                        .map_err(|_| todo!())
                        .unwrap()
                    {
                        None => me.state.set(SendDataState::Closing),
                        Some(h) => {
                            me.state.set(SendDataState::Trailers(SendHeaders::new(
                                Header::trailer(h),
                                &me.conn,
                                me.send.take().expect("send"),
                                *me.stream_id,
                            )?));
                        }
                    }
                }
                SendDataState::Trailers(send) => {
                    *me.send = Some(ready!(Pin::new(send).poll(cx))?);
                    me.state.set(SendDataState::Closing);
                }
                SendDataState::Closing => {
                    ready!(Pin::new(me.send.as_mut().unwrap()).poll_finish(cx))?;
                    if *me.finish {
                        let mut conn = me.conn.h3.lock().unwrap();
                        conn.inner.request_finished(*me.stream_id);
                    }
                    return Poll::Ready(Ok(()));
                }
                SendDataState::Finished => return Poll::Ready(Ok(())),
            };
        }
    }
}

pub struct RecvData {
    state: RecvDataState,
    conn: ConnectionRef,
    recv: Option<FrameStream>,
    stream_id: StreamId,
}

enum RecvDataState {
    Receiving,
    Decoding(DecodeHeaders),
    Finished,
}

impl RecvData {
    pub(crate) fn new(recv: FrameStream, conn: ConnectionRef, stream_id: StreamId) -> Self {
        Self {
            conn,
            stream_id,
            recv: Some(recv),
            state: RecvDataState::Receiving,
        }
    }

    pub fn reset(&mut self, err_code: ErrorCode) {
        if let Some(ref mut r) = self.recv {
            r.reset(err_code);
        }
    }
}

impl Future for RecvData {
    type Output = Result<(Header, RecvBody), Error>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match &mut self.state {
                RecvDataState::Receiving => {
                    match ready!(Pin::new(self.recv.as_mut().unwrap()).poll_next(cx)) {
                        Some(Ok(HttpFrame::Reserved)) => continue,
                        Some(Ok(HttpFrame::Headers(h))) => {
                            self.state = RecvDataState::Decoding(DecodeHeaders::new(
                                h,
                                self.conn.clone(),
                                self.stream_id,
                            ));
                        }
                        Some(Err(e)) => {
                            self.recv.as_mut().unwrap().reset(e.code());
                            return Poll::Ready(Err(e.into()));
                        }
                        Some(Ok(f)) => {
                            self.recv
                                .as_mut()
                                .unwrap()
                                .reset(ErrorCode::FRAME_UNEXPECTED);
                            return Poll::Ready(Err(Error::Peer(format!(
                                "First frame is not headers: {:?}",
                                f
                            ))));
                        }
                        None => {
                            return Poll::Ready(Err(Error::peer("Stream end unexpected")));
                        }
                    };
                }
                RecvDataState::Decoding(ref mut decode) => {
                    let headers = ready!(Pin::new(decode).poll(cx))?;
                    let recv =
                        RecvBody::new(self.conn.clone(), self.stream_id, self.recv.take().unwrap());
                    self.state = RecvDataState::Finished;
                    return Poll::Ready(Ok((headers, recv)));
                }
                RecvDataState::Finished => panic!("polled after finished"),
            }
        }
    }
}
