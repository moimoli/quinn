use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use quinn::SendStream;
use quinn_proto::StreamId;

use crate::{
    connection::ConnectionRef,
    frame::WriteFrame,
    proto::{frame::HeadersFrame, headers::Header, ErrorCode},
    Error,
};

pub struct DecodeHeaders {
    frame: Option<HeadersFrame>,
    conn: ConnectionRef,
    stream_id: StreamId,
}

impl DecodeHeaders {
    pub(crate) fn new(frame: HeadersFrame, conn: ConnectionRef, stream_id: StreamId) -> Self {
        Self {
            conn,
            stream_id,
            frame: Some(frame),
        }
    }
}

impl Future for DecodeHeaders {
    type Output = Result<Header, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match self.frame {
            None => Poll::Ready(Err(crate::Error::internal("frame none"))),
            Some(ref frame) => {
                let mut conn = self.conn.h3.lock().unwrap();
                conn.poll_decode(cx, self.stream_id, frame)
            }
        }
    }
}

pub(crate) struct SendHeaders(WriteFrame<HeadersFrame>);

impl SendHeaders {
    pub fn new(
        header: Header,
        conn: &ConnectionRef,
        send: SendStream,
        stream_id: StreamId,
    ) -> Result<Self, Error> {
        let conn = &mut conn.h3.lock().unwrap();
        let frame = conn.inner.encode_header(stream_id, header)?;
        conn.wake();

        Ok(Self(WriteFrame::new(send, frame)))
    }

    pub fn reset(&mut self, err_code: ErrorCode) {
        self.0.reset(err_code);
    }

    pub fn poll_stopped(&mut self, cx: &mut Context) -> Poll<Option<ErrorCode>> {
        self.0.poll_stopped(cx)
    }
}

impl<'a> Future for SendHeaders {
    type Output = Result<SendStream, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map_err(Into::into)
    }
}
