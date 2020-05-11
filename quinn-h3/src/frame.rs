use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, BufMut, BytesMut};
use futures::{ready, FutureExt};
use pin_project::{pin_project, project};
use quinn::{RecvStream, SendStream, VarInt};
use tokio::io::AsyncRead;
use tokio_util::codec::{Decoder, FramedRead};

use super::proto::frame::{self, FrameHeader, HttpFrame, IntoPayload, PartialData};
use crate::{proto::ErrorCode, streams::Reset};
pub type FrameStream = FramedRead<RecvStream, FrameDecoder>;

impl Reset for FrameStream {
    fn reset(&mut self, error_code: ErrorCode) {
        let _ = self.get_mut().stop(error_code.0.into());
    }
}

#[derive(Default)]
pub struct FrameDecoder {
    partial: Option<PartialData>,
    expected: Option<usize>,
}

impl FrameDecoder {
    pub fn stream<T: AsyncRead>(stream: T) -> FramedRead<T, Self> {
        FramedRead::with_capacity(
            stream,
            FrameDecoder {
                expected: None,
                partial: None,
            },
            65535,
        )
    }
}

macro_rules! decode {
    ($buf:ident, $dec:expr) => {{
        let mut cur = io::Cursor::new(&$buf);
        let decoded = $dec(&mut cur);
        (cur.position() as usize, decoded)
    }};
}

impl Decoder for FrameDecoder {
    type Item = HttpFrame;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.is_empty() {
            return Ok(None);
        }

        if let Some(ref mut partial) = self.partial {
            let (pos, frame) = decode!(src, |cur| HttpFrame::Data(partial.decode_data(cur)));
            src.advance(pos);

            if partial.remaining() == 0 {
                self.partial = None;
            }

            return Ok(Some(frame));
        }

        if let Some(min) = self.expected {
            if src.len() < min {
                return Ok(None);
            }
        }

        let (pos, decoded) = decode!(src, |cur| HttpFrame::decode(cur));

        match decoded {
            Err(frame::Error::IncompleteData) => {
                let (pos, decoded) = decode!(src, |cur| PartialData::decode(cur));
                let (partial, frame) = decoded?;
                src.advance(pos);
                self.expected = None;
                self.partial = Some(partial);
                if frame.len() > 0 {
                    Ok(Some(HttpFrame::Data(frame)))
                } else {
                    Ok(None)
                }
            }
            Err(frame::Error::Incomplete(min)) => {
                self.expected = Some(min);
                Ok(None)
            }
            Err(e) => Err(e.into()),
            Ok(frame) => {
                src.advance(pos);
                self.expected = None;
                Ok(Some(frame))
            }
        }
    }
}

#[pin_project]
pub(crate) struct WriteFrame<F> {
    state: WriteFrameState,
    #[pin]
    send: Option<SendStream>,
    frame: F,
    header: [u8; VarInt::MAX_SIZE * 2],
    header_len: usize,
}

enum WriteFrameState {
    Header(usize),
    Payload,
    Finished,
}

impl<F> WriteFrame<F>
where
    F: FrameHeader + IntoPayload,
{
    pub(crate) fn new(send: SendStream, frame: F) -> Self {
        let mut buf = [0u8; VarInt::MAX_SIZE * 2];
        let remaining = {
            let mut cur = &mut buf[..];
            frame.encode_header(&mut cur);
            cur.remaining_mut()
        };

        Self {
            frame,
            send: Some(send),
            state: WriteFrameState::Header(0),
            header: buf,
            header_len: buf.len() - remaining,
        }
    }

    pub fn reset(&mut self, err_code: ErrorCode) {
        if let Some(ref mut s) = self.send {
            s.reset(err_code.into());
        }
    }

    pub fn poll_stopped(&mut self, cx: &mut Context) -> Poll<Option<ErrorCode>> {
        match self.send {
            Some(ref mut s) => Poll::Ready(ready!(s.poll_stopped(cx)).map(Into::into)),
            None => Poll::Pending,
        }
    }
}

impl<F> Future for WriteFrame<F>
where
    F: FrameHeader + IntoPayload,
{
    type Output = Result<SendStream, quinn::WriteError>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut me = self.project();
        loop {
            match me.state {
                WriteFrameState::Finished => panic!("polled after finish"),
                WriteFrameState::Header(mut start) => {
                    let mut send = me.send.as_mut();
                    let send = (*send).as_mut().unwrap();
                    let wrote = ready!(send
                        .write(&me.header[start..*me.header_len])
                        .poll_unpin(cx)?);
                    start += wrote;

                    if start < *me.header_len {
                        *me.state = WriteFrameState::Header(start);
                        continue;
                    }
                    *me.state = WriteFrameState::Payload;
                }
                WriteFrameState::Payload => {
                    let mut send = me.send.as_mut().take().unwrap();
                    let p = me.frame.into_payload();

                    match send.write(p.bytes()).poll_unpin(cx) {
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => {
                            me.send.set(Some(send));
                            return Poll::Pending;
                        }
                        Poll::Ready(Ok(wrote)) => {
                            p.advance(wrote);
                            if p.has_remaining() {
                                me.send.set(Some(send));
                                continue;
                            }
                        }
                    }

                    *me.state = WriteFrameState::Finished;
                    return Poll::Ready(Ok(send));
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Proto(frame::Error),
    Io(io::Error),
}

impl Error {
    pub fn code(&self) -> ErrorCode {
        match self {
            Error::Io(_) => ErrorCode::GENERAL_PROTOCOL_ERROR,
            Error::Proto(frame::Error::Settings(_)) => ErrorCode::SETTINGS_ERROR,
            Error::Proto(frame::Error::UnsupportedFrame(_)) => ErrorCode::FRAME_UNEXPECTED,
            Error::Proto(_) => ErrorCode::FRAME_ERROR,
        }
    }
}

impl From<frame::Error> for Error {
    fn from(err: frame::Error) -> Self {
        Error::Proto(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::frame;

    #[test]
    fn one_frame() {
        let frame = frame::HeadersFrame {
            encoded: b"salut"[..].into(),
        };

        let mut buf = BytesMut::with_capacity(16);
        frame.encode(&mut buf);

        let mut decoder = FrameDecoder::default();
        assert_matches!(decoder.decode(&mut buf), Ok(Some(HttpFrame::Headers(_))));
    }

    #[test]
    fn incomplete_frame() {
        let frame = frame::HeadersFrame {
            encoded: b"salut"[..].into(),
        };

        let mut buf = BytesMut::with_capacity(16);
        frame.encode(&mut buf);
        buf.truncate(buf.len() - 1);

        let mut decoder = FrameDecoder::default();
        assert_matches!(decoder.decode(&mut buf), Ok(None));
    }

    #[test]
    fn two_frames_then_incomplete() {
        let frames = [
            HttpFrame::Headers(frame::HeadersFrame {
                encoded: b"header"[..].into(),
            }),
            HttpFrame::Data(frame::DataFrame {
                payload: b"body"[..].into(),
            }),
            HttpFrame::Headers(frame::HeadersFrame {
                encoded: b"trailer"[..].into(),
            }),
        ];

        let mut buf = BytesMut::with_capacity(64);
        for frame in frames.iter() {
            frame.encode(&mut buf);
        }
        buf.truncate(buf.len() - 1);

        let mut decoder = FrameDecoder::default();
        assert_matches!(decoder.decode(&mut buf), Ok(Some(HttpFrame::Headers(_))));
        assert_matches!(decoder.decode(&mut buf), Ok(Some(HttpFrame::Data(_))));
        assert_matches!(decoder.decode(&mut buf), Ok(None));
    }
}
