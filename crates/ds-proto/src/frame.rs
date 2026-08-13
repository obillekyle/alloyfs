use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::ProtoError;
use crate::messages::Frame;

/// Hard upper bound for one frame's payload. Anything larger is a protocol
/// violation: data is chunked at 128 KiB, so a legitimate frame never gets
/// close. Protects both sides from corrupt lengths and hostile peers.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

const LEN_PREFIX: usize = 4;

/// Wire format: `u32 little-endian payload length` + `postcard(Frame)`.
#[derive(Debug, Default)]
pub struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = ProtoError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, ProtoError> {
        if src.len() < LEN_PREFIX {
            return Ok(None); // not even a length yet — wait for more bytes
        }
        let len = u32::from_le_bytes(src[..LEN_PREFIX].try_into().unwrap()) as usize;
        if len > MAX_FRAME_LEN {
            return Err(ProtoError::FrameTooLarge(len));
        }
        if src.len() < LEN_PREFIX + len {
            // Reserve what we still need so the next read can complete the frame.
            src.reserve(LEN_PREFIX + len - src.len());
            return Ok(None);
        }
        src.advance(LEN_PREFIX);
        let payload = src.split_to(len);
        Ok(Some(postcard::from_bytes(&payload)?))
    }
}

impl Encoder<&Frame> for FrameCodec {
    type Error = ProtoError;

    fn encode(&mut self, frame: &Frame, dst: &mut BytesMut) -> Result<(), ProtoError> {
        let payload = postcard::to_stdvec(frame)?;
        if payload.len() > MAX_FRAME_LEN {
            return Err(ProtoError::FrameTooLarge(payload.len()));
        }
        dst.reserve(LEN_PREFIX + payload.len());
        dst.put_u32_le(payload.len() as u32);
        dst.put_slice(&payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::*;
    use bytes::Bytes;
    use std::time::SystemTime;

    fn roundtrip(frame: &Frame) -> Frame {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        codec.encode(frame, &mut buf).expect("encode");
        codec.decode(&mut buf).expect("decode").expect("complete frame")
    }

    fn sample_attr() -> Attr {
        Attr {
            kind: FileKind::File,
            size: 42,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
            mode: 0o644,
            version: 7,
        }
    }

    #[test]
    fn roundtrip_all_variants() {
        let frames = vec![
            Frame::Hello { proto_min: 1, proto_max: 1, client: "test".into() },
            Frame::HelloAck { proto: 1, server: "srv".into() },
            Frame::Request {
                id: 9,
                body: Request::Write {
                    fh: 3,
                    offset: 128 * 1024,
                    data: Bytes::from_static(b"hello world"),
                    expect_version: Some(6),
                },
            },
            Frame::Response { id: 9, body: Ok(Response::Written { n: 11, new_version: 7, conflict: false }) },
            Frame::Response { id: 10, body: Err(crate::ErrorCode::NotFound) },
            Frame::Events {
                batch: vec![FsEvent {
                    seq: 55,
                    kind: EventKind::RenamedFrom { to: RelPath("b/new.txt".into()) },
                    path: RelPath("a/old.txt".into()),
                    new_version: Some(8),
                    origin: Some(1),
                }],
            },
            Frame::Ping { nonce: 123 },
            Frame::Pong { nonce: 123 },
            Frame::Response {
                id: 2,
                body: Ok(Response::Dir {
                    entries: vec![DirEntry { name: "x".into(), attr: sample_attr() }],
                    next_cursor: None,
                }),
            },
        ];
        for frame in &frames {
            // Frame doesn't impl PartialEq (Bytes payloads); compare wire images.
            let redecoded = roundtrip(frame);
            let a = postcard::to_stdvec(frame).unwrap();
            let b = postcard::to_stdvec(&redecoded).unwrap();
            assert_eq!(a, b, "roundtrip changed {frame:?}");
        }
    }

    #[test]
    fn truncated_frame_waits_for_more() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        codec.encode(&Frame::Ping { nonce: 1 }, &mut buf).unwrap();
        let full = buf.clone();
        for cut in 0..full.len() {
            let mut partial = BytesMut::from(&full[..cut]);
            assert!(matches!(codec.decode(&mut partial), Ok(None)), "cut at {cut}");
        }
    }

    #[test]
    fn oversize_frame_rejected() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        buf.put_u32_le((MAX_FRAME_LEN + 1) as u32);
        buf.put_slice(&[0u8; 16]);
        assert!(matches!(codec.decode(&mut buf), Err(ProtoError::FrameTooLarge(_))));
    }

    #[test]
    fn garbage_payload_is_malformed() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        buf.put_u32_le(4);
        buf.put_slice(&[0xff, 0xff, 0xff, 0xff]);
        assert!(matches!(codec.decode(&mut buf), Err(ProtoError::Malformed(_))));
    }

    #[test]
    fn two_frames_in_one_buffer() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        codec.encode(&Frame::Ping { nonce: 1 }, &mut buf).unwrap();
        codec.encode(&Frame::Pong { nonce: 2 }, &mut buf).unwrap();
        assert!(matches!(codec.decode(&mut buf), Ok(Some(Frame::Ping { nonce: 1 }))));
        assert!(matches!(codec.decode(&mut buf), Ok(Some(Frame::Pong { nonce: 2 }))));
        assert!(matches!(codec.decode(&mut buf), Ok(None)));
    }

    #[test]
    fn relpath_validation() {
        assert!(RelPath("a/b/c.txt".into()).validate().is_ok());
        assert!(RelPath(String::new()).validate().is_ok());
        assert!(RelPath("/abs".into()).validate().is_err());
        assert!(RelPath("a/../b".into()).validate().is_err());
        assert!(RelPath("a\\b".into()).validate().is_err());
        assert!(RelPath("a//b".into()).validate().is_err());
        assert!(RelPath(".".into()).validate().is_err());
    }
}
