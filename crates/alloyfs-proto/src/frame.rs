use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::ProtoError;
use crate::messages::Frame;

/// Hard upper bound for one frame's payload. Anything larger is a protocol
/// violation: data is chunked at 128 KiB, so a legitimate frame never gets
/// close. Protects both sides from corrupt lengths and hostile peers.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

/// Frames smaller than this aren't worth compressing: the lz4 header plus
/// the `Compressed` wrapper would eat most of the win.
const COMPRESS_MIN: usize = 512;

const LEN_PREFIX: usize = 4;

/// Wire format: `u32 little-endian payload length` + `postcard(Frame)`.
///
/// With `compress` on (both peers negotiated protocol v3+), outgoing frames
/// of COMPRESS_MIN+ bytes are lz4-compressed and wrapped in
/// `Frame::Compressed` — but only when that actually made them smaller, so
/// already-compressed file data passes through untouched. The decoder always
/// unwraps `Compressed` regardless of the flag; the version gate lives on
/// the sending side.
#[derive(Debug, Default)]
pub struct FrameCodec {
    pub compress: bool,
}

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
        match postcard::from_bytes(&payload)? {
            Frame::Compressed(data) => {
                // The prepended size is attacker-controlled: bound it before
                // allocating anything (decompression-bomb guard).
                if data.len() >= 4 {
                    let claimed = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
                    if claimed > MAX_FRAME_LEN {
                        return Err(ProtoError::FrameTooLarge(claimed));
                    }
                }
                let inner = lz4_flex::decompress_size_prepended(&data)
                    .map_err(|e| ProtoError::Compression(e.to_string()))?;
                match postcard::from_bytes(&inner)? {
                    Frame::Compressed(_) => Err(ProtoError::Compression("nested Compressed frame".into())),
                    frame => Ok(Some(frame)),
                }
            }
            frame => Ok(Some(frame)),
        }
    }
}

impl Encoder<&Frame> for FrameCodec {
    type Error = ProtoError;

    fn encode(&mut self, frame: &Frame, dst: &mut BytesMut) -> Result<(), ProtoError> {
        let mut payload = postcard::to_stdvec(frame)?;
        if self.compress && payload.len() >= COMPRESS_MIN && !matches!(frame, Frame::Compressed(_)) {
            let squeezed = lz4_flex::compress_prepend_size(&payload);
            if squeezed.len() < payload.len() {
                payload = postcard::to_stdvec(&Frame::Compressed(Bytes::from(squeezed)))?;
            }
        }
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
        let mut codec = FrameCodec::default();
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
            Frame::Hello {
                proto_min: 1,
                proto_max: 1,
                client: "test".into(),
            },
            Frame::HelloAck {
                proto: 1,
                server: "srv".into(),
            },
            Frame::Request {
                id: 9,
                body: Request::Write {
                    fh: 3,
                    offset: 128 * 1024,
                    data: Bytes::from_static(b"hello world"),
                    expect_version: Some(6),
                },
            },
            Frame::Response {
                id: 9,
                body: Ok(Response::Written {
                    n: 11,
                    new_version: 7,
                    conflict: false,
                }),
            },
            Frame::Response {
                id: 10,
                body: Err(crate::ErrorCode::NotFound),
            },
            Frame::Events {
                batch: vec![FsEvent {
                    seq: 55,
                    kind: EventKind::RenamedFrom {
                        to: RelPath("b/new.txt".into()),
                    },
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
                    entries: vec![DirEntry {
                        name: "x".into(),
                        attr: sample_attr(),
                    }],
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
    fn compression_roundtrips_and_shrinks() {
        let mut tx = FrameCodec { compress: true };
        let mut rx = FrameCodec::default(); // decode side needs no flag
        let data = Bytes::from(vec![0x42u8; 128 * 1024]); // maximally compressible
        let frame = Frame::Response {
            id: 7,
            body: Ok(Response::Data(data.clone())),
        };
        let mut buf = BytesMut::new();
        tx.encode(&frame, &mut buf).unwrap();
        assert!(
            buf.len() < data.len() / 10,
            "128K of constant bytes should shrink dramatically, got {}",
            buf.len()
        );
        match rx.decode(&mut buf).unwrap().unwrap() {
            Frame::Response {
                id: 7,
                body: Ok(Response::Data(got)),
            } => assert_eq!(got, data),
            other => panic!("wrong frame after decompression: {other:?}"),
        }
    }

    #[test]
    fn incompressible_data_passes_through_uncompressed() {
        // Pseudo-random bytes don't shrink; the encoder must fall back to the
        // plain encoding rather than growing the frame.
        let mut noise = vec![0u8; 64 * 1024];
        let mut state = 0x12345678u32;
        for b in noise.iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        let frame = Frame::Response {
            id: 1,
            body: Ok(Response::Data(Bytes::from(noise.clone()))),
        };
        let plain_len = postcard::to_stdvec(&frame).unwrap().len();
        let mut tx = FrameCodec { compress: true };
        let mut buf = BytesMut::new();
        tx.encode(&frame, &mut buf).unwrap();
        assert_eq!(
            buf.len(),
            LEN_PREFIX + plain_len,
            "must not wrap when lz4 doesn't win"
        );
        match FrameCodec::default().decode(&mut buf).unwrap().unwrap() {
            Frame::Response {
                body: Ok(Response::Data(got)),
                ..
            } => assert_eq!(&got[..], &noise[..]),
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn compression_bomb_rejected() {
        // A Compressed frame claiming to inflate past MAX_FRAME_LEN must be
        // rejected before any allocation happens.
        let mut bomb = Vec::new();
        bomb.extend_from_slice(&(u32::MAX).to_le_bytes());
        bomb.extend_from_slice(&[0u8; 32]);
        let frame = Frame::Compressed(Bytes::from(bomb));
        let mut buf = BytesMut::new();
        FrameCodec::default().encode(&frame, &mut buf).unwrap();
        assert!(matches!(
            FrameCodec::default().decode(&mut buf),
            Err(ProtoError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn truncated_frame_waits_for_more() {
        let mut codec = FrameCodec::default();
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
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32_le((MAX_FRAME_LEN + 1) as u32);
        buf.put_slice(&[0u8; 16]);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(ProtoError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn garbage_payload_is_malformed() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32_le(4);
        buf.put_slice(&[0xff, 0xff, 0xff, 0xff]);
        assert!(matches!(codec.decode(&mut buf), Err(ProtoError::Malformed(_))));
    }

    #[test]
    fn two_frames_in_one_buffer() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(&Frame::Ping { nonce: 1 }, &mut buf).unwrap();
        codec.encode(&Frame::Pong { nonce: 2 }, &mut buf).unwrap();
        assert!(matches!(
            codec.decode(&mut buf),
            Ok(Some(Frame::Ping { nonce: 1 }))
        ));
        assert!(matches!(
            codec.decode(&mut buf),
            Ok(Some(Frame::Pong { nonce: 2 }))
        ));
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

        // A Windows drive reference in any component. `C:evil.txt` is the one
        // that mattered: `PathBuf::join` REPLACES its buffer when the argument
        // carries a drive prefix, so a leaf spliced onto an already-checked
        // parent resolves against drive C's working directory instead of the
        // export — while still reporting `is_absolute() == false`.
        assert!(RelPath("C:evil.txt".into()).validate().is_err());
        assert!(RelPath("sub/C:evil.txt".into()).validate().is_err());
        assert!(RelPath("C:".into()).validate().is_err());
        assert!(RelPath("a:b:c".into()).validate().is_err());

        // ...and NOT an ordinary colon, which is legal in a POSIX filename and
        // is not a drive prefix unless a single letter precedes it. Rejecting
        // these would take out every ISO-timestamped log on a Linux export.
        assert!(RelPath("log-10:30:00.txt".into()).validate().is_ok());
        assert!(RelPath("foo:bar".into()).validate().is_ok());
    }

    /// Decode rejects what a peer must never be able to say, so a path
    /// arriving FROM a server — an event, a listing, a tree entry — cannot
    /// carry traversal into whatever the receiver builds out of it.
    #[test]
    fn decode_rejects_a_traversing_relpath() {
        for bad in ["../../etc/passwd", "a/../b", "C:evil.txt", "/abs", "."] {
            let bytes = postcard::to_stdvec(&bad.to_string()).unwrap();
            assert!(
                postcard::from_bytes::<RelPath>(&bytes).is_err(),
                "decode accepted {bad:?}"
            );
        }
        // A backslash is NOT rejected here, only by the server's stricter
        // `validate`. It is a legal character in a POSIX filename, and a
        // decode rule that fired on one would drop the connection every time
        // an event mentioned that file — a reconnect loop, not a safety net.
        for ok in ["a/b/c.txt", "log-10:30:00.txt", "a\\b", ""] {
            let bytes = postcard::to_stdvec(&ok.to_string()).unwrap();
            assert_eq!(
                postcard::from_bytes::<RelPath>(&bytes).unwrap(),
                RelPath(ok.into()),
                "decode rejected {ok:?}"
            );
        }
    }
}
