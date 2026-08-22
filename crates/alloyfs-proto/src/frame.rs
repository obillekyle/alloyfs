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

/// zstd level for `Frame::Zstd`. 3 is the crate default and the sweet spot
/// for a 28 Mbit/s-class link on this CPU class: substantially better
/// ratio than lz4 while still compressing far faster than the link drains.
const ZSTD_LEVEL: i32 = 3;

const LEN_PREFIX: usize = 4;

/// Wire format: `u32 little-endian payload length` + `postcard(Frame)`.
///
/// With `compress` on (both peers negotiated protocol v3+), outgoing frames
/// of COMPRESS_MIN+ bytes are lz4-compressed and wrapped in
/// `Frame::Compressed` — but only when that actually made them smaller, so
/// already-compressed file data passes through untouched. With `zstd` also
/// flipped (negotiated v13+ AND the sender's config opted in), zstd takes
/// lz4's place under the same rules. The decoder always unwraps both
/// wrappers regardless of any flag; every version gate lives on the
/// sending side.
#[derive(Default)]
pub struct FrameCodec {
    pub compress: bool,
    /// Shared and atomic because the codec moves into the writer task at
    /// establish time, before the sender knows whether ITS config wants
    /// zstd — the connection keeps a clone and flips it after the
    /// handshake. Never set below v13.
    pub zstd: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Reused zstd contexts. `zstd::bulk::compress`/`decompress` build a
    /// full CCtx/DCtx — hundreds of KB of tables at level 3 — on every
    /// call, which at one call per 128 KiB frame was a measurable slice of
    /// a 2-core agent's CPU. Each codec half is owned by exactly one task,
    /// so plain fields need no synchronization; lazy so `Default` stays
    /// derivable and a codec that never sees zstd allocates nothing.
    cctx: Option<zstd::bulk::Compressor<'static>>,
    dctx: Option<zstd::bulk::Decompressor<'static>>,
}

impl std::fmt::Debug for FrameCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameCodec")
            .field("compress", &self.compress)
            .field("zstd", &self.zstd.load(std::sync::atomic::Ordering::Relaxed))
            .finish_non_exhaustive()
    }
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
                    Frame::Compressed(_) | Frame::Zstd(_) => {
                        Err(ProtoError::Compression("nested compressed frame".into()))
                    }
                    frame => Ok(Some(frame)),
                }
            }
            Frame::Zstd(data) => {
                // Same prepended-size layout and the same bomb guard as lz4;
                // the size here is also the exact decompression budget.
                if data.len() < 4 {
                    return Err(ProtoError::Compression("zstd frame too short".into()));
                }
                let claimed = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
                if claimed > MAX_FRAME_LEN {
                    return Err(ProtoError::FrameTooLarge(claimed));
                }
                let dctx = match &mut self.dctx {
                    Some(c) => c,
                    None => self.dctx.insert(
                        zstd::bulk::Decompressor::new()
                            .map_err(|e| ProtoError::Compression(e.to_string()))?,
                    ),
                };
                let inner = dctx
                    .decompress(&data[4..], claimed)
                    .map_err(|e| ProtoError::Compression(e.to_string()))?;
                match postcard::from_bytes(&inner)? {
                    Frame::Compressed(_) | Frame::Zstd(_) => {
                        Err(ProtoError::Compression("nested compressed frame".into()))
                    }
                    frame => Ok(Some(frame)),
                }
            }
            frame => Ok(Some(frame)),
        }
    }
}

impl Encoder<&Frame> for FrameCodec {
    type Error = ProtoError;

    /// Serialize STRAIGHT INTO the output buffer, with the length written
    /// afterwards over a placeholder.
    ///
    /// The frame's bytes are unchanged — the golden vectors say so — but
    /// the trip they take is shorter. Every frame used to be serialized to
    /// its own `Vec` and then copied into `dst`; a compressed one was
    /// serialized, compressed, copied into a size-prefixed `Vec`, then
    /// serialized AGAIN through postcard purely to prepend a variant tag
    /// and a length, and finally copied into `dst`. For a 128 KiB read
    /// reply — the shape the agent sends most — that was up to five passes
    /// over the payload. Now it is one for a plain frame and three for a
    /// compressed one, and the intermediate `Vec` per frame is gone.
    fn encode(&mut self, frame: &Frame, dst: &mut BytesMut) -> Result<(), ProtoError> {
        let start = dst.len();
        dst.put_u32_le(0); // length placeholder, backfilled below
        if let Err(e) = postcard::to_io(frame, (&mut *dst).writer()) {
            dst.truncate(start); // never leave half a frame behind
            return Err(e.into());
        }
        let plain_len = dst.len() - start - LEN_PREFIX;

        let wrapper = matches!(frame, Frame::Compressed(_) | Frame::Zstd(_));
        let zstd_on = self.zstd.load(std::sync::atomic::Ordering::Relaxed);
        if !wrapper && plain_len >= COMPRESS_MIN {
            // Compress out of the bytes already in `dst`, then replace them
            // with the wrapper only if that actually won.
            let squeezed = if zstd_on {
                // A context that fails to build skips compression for this
                // frame — the same silent fallback a failed compress took.
                let cctx = match &mut self.cctx {
                    Some(c) => Some(c),
                    None => zstd::bulk::Compressor::new(ZSTD_LEVEL)
                        .ok()
                        .map(|c| self.cctx.insert(c)),
                };
                let plain = &dst[start + LEN_PREFIX..];
                // Same layout as lz4's: the uncompressed size, prepended.
                cctx.and_then(|c| c.compress(plain).ok())
                    .filter(|s| s.len() + 4 < plain.len())
                    .map(|s| {
                        let mut framed = Vec::with_capacity(s.len() + 4);
                        framed.extend_from_slice(&(plain.len() as u32).to_le_bytes());
                        framed.extend_from_slice(&s);
                        Frame::Zstd(Bytes::from(framed))
                    })
            } else if self.compress {
                let plain = &dst[start + LEN_PREFIX..];
                let s = lz4_flex::compress_prepend_size(plain);
                (s.len() < plain.len()).then(|| Frame::Compressed(Bytes::from(s)))
            } else {
                None
            };
            if let Some(wrapped) = squeezed {
                dst.truncate(start + LEN_PREFIX);
                if let Err(e) = postcard::to_io(&wrapped, (&mut *dst).writer()) {
                    dst.truncate(start);
                    return Err(e.into());
                }
            }
        }

        let payload_len = dst.len() - start - LEN_PREFIX;
        if payload_len > MAX_FRAME_LEN {
            dst.truncate(start);
            return Err(ProtoError::FrameTooLarge(payload_len));
        }
        dst[start..start + LEN_PREFIX].copy_from_slice(&(payload_len as u32).to_le_bytes());
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
        let mut tx = FrameCodec {
            compress: true,
            ..FrameCodec::default()
        };
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
        let mut tx = FrameCodec {
            compress: true,
            ..FrameCodec::default()
        };
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

    fn zstd_codec() -> FrameCodec {
        let c = FrameCodec {
            compress: true, // both flags on, like a real v13 opted-in sender
            ..FrameCodec::default()
        };
        c.zstd.store(true, std::sync::atomic::Ordering::Relaxed);
        c
    }

    /// The zstd mirror of `compression_roundtrips_and_shrinks`, plus the
    /// preference rule: with both flags on, zstd is what goes out.
    #[test]
    fn zstd_roundtrips_shrinks_and_wins_over_lz4() {
        let mut tx = zstd_codec();
        let mut rx = FrameCodec::default(); // decode side needs no flag
        let data = Bytes::from(vec![0x42u8; 128 * 1024]);
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
        // The wire must carry the Zstd variant, not lz4's Compressed — peek
        // by decoding the OUTER frame without unwrapping.
        let outer: Frame = postcard::from_bytes(&buf[LEN_PREFIX..]).unwrap();
        assert!(matches!(outer, Frame::Zstd(_)), "zstd must take precedence");
        match rx.decode(&mut buf).unwrap().unwrap() {
            Frame::Response {
                id: 7,
                body: Ok(Response::Data(got)),
            } => assert_eq!(got, data),
            other => panic!("wrong frame after decompression: {other:?}"),
        }
    }

    /// Same only-when-smaller rule as lz4: noise passes through plain.
    #[test]
    fn zstd_incompressible_passes_through_uncompressed() {
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
        let mut tx = zstd_codec();
        let mut buf = BytesMut::new();
        tx.encode(&frame, &mut buf).unwrap();
        assert_eq!(
            buf.len(),
            LEN_PREFIX + plain_len,
            "must not wrap when zstd doesn't win"
        );
    }

    /// The bomb guard: a Zstd frame whose prepended size exceeds the frame
    /// cap is refused before any allocation, exactly like lz4's.
    #[test]
    fn zstd_bomb_claim_is_refused() {
        let mut inner = Vec::new();
        inner.extend_from_slice(&(MAX_FRAME_LEN as u32 + 1).to_le_bytes());
        inner.extend_from_slice(b"whatever");
        let hostile = Frame::Zstd(Bytes::from(inner));
        let payload = postcard::to_stdvec(&hostile).unwrap();
        let mut buf = BytesMut::new();
        buf.put_u32_le(payload.len() as u32);
        buf.put_slice(&payload);
        match FrameCodec::default().decode(&mut buf) {
            Err(ProtoError::FrameTooLarge(n)) => assert!(n > MAX_FRAME_LEN),
            other => panic!("bomb claim must be refused, got {other:?}"),
        }
    }
}
