//! The frozen kernel <-> daemon ABI, in Rust.
//!
//! Mirrors `kernel/alloyfs/uapi/alloyfs.h` field for field. Every struct there
//! is fixed-size and naturally aligned, so this module encodes and decodes by
//! hand from byte slices in NATIVE byte order (both ends are the same machine)
//! rather than transmuting `repr(C)` structs — no unsafe, no alignment
//! assumptions about a buffer the kernel filled, and it compiles and tests on
//! any platform.
//!
//! The layout constants below are asserted against the C header by
//! `kernel/test/tools/alloyfs-abi-check.c`; changing one without changing the
//! header is a wire break.

#![allow(dead_code)] // some constants exist to document the ABI, not to be used

pub const ABI_VERSION: u32 = 1;
pub const ROOT_NODEID: u64 = 1;

/// Largest payload either direction; bounds every kernel-side allocation.
pub const MAX_PAYLOAD: usize = 128 * 1024;
pub const MAX_NAME: usize = 255;

pub const IN_HEADER_LEN: usize = 40;
pub const OUT_HEADER_LEN: usize = 16;
pub const ATTR_LEN: usize = 32;
pub const CREATE_IN_LEN: usize = 8;
pub const RENAME_IN_LEN: usize = 16;
pub const SETATTR_IN_LEN: usize = 24;
pub const WRITE_OUT_LEN: usize = 8;
pub const NOTIFY_ENTRY_LEN: usize = 32;
pub const DIRENT_LEN: usize = 24;

/// `alloyfs_dev_read` rejects anything smaller with EINVAL: one read() must be
/// able to hold ANY request, so a 128 KiB write never spans two reads.
pub const READ_BUF_LEN: usize = IN_HEADER_LEN + MAX_PAYLOAD;

// ------------------------------------------------------------------ opcodes

pub const OP_LOOKUP: u32 = 1;
pub const OP_GETATTR: u32 = 2;
pub const OP_READDIR: u32 = 3;
pub const OP_READ: u32 = 4;
pub const OP_CREATE: u32 = 5;
pub const OP_MKDIR: u32 = 6;
pub const OP_UNLINK: u32 = 7;
pub const OP_RMDIR: u32 = 8;
pub const OP_RENAME: u32 = 9;
pub const OP_WRITE: u32 = 10;
pub const OP_SETATTR: u32 = 11;

pub const SETATTR_MODE: u32 = 1 << 0;
pub const SETATTR_SIZE: u32 = 1 << 1;
pub const SETATTR_MTIME: u32 = 1 << 2;

// ------------------------------------------------------------ notifications

pub const NOTIFY_CREATE: i32 = 1;
pub const NOTIFY_DELETE: i32 = 2;
pub const NOTIFY_MODIFY: i32 = 3;
pub const NOTIFY_ATTRIB: i32 = 4;
pub const NOTIFY_RENAME: i32 = 5;

pub const NOTIFY_F_ISDIR: u32 = 1 << 0;

// ------------------------------------------------------- mode bits / dirents

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;

pub const DT_DIR: u32 = 4;
pub const DT_REG: u32 = 8;

// -------------------------------------------------------------------- errnos
//
// Linux values, spelled out so the cross-platform half of this crate needs no
// libc.

pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const EIO: i32 = 5;
pub const EBADF: i32 = 9;
pub const EAGAIN: i32 = 11;
pub const EACCES: i32 = 13;
pub const EEXIST: i32 = 17;
pub const EXDEV: i32 = 18;
pub const ENODEV: i32 = 19;
pub const ENOTDIR: i32 = 20;
pub const EISDIR: i32 = 21;
pub const EINVAL: i32 = 22;
pub const EFBIG: i32 = 27;
pub const EROFS: i32 = 30;
pub const ENAMETOOLONG: i32 = 36;
pub const ENOSYS: i32 = 38;
pub const ENOTEMPTY: i32 = 39;

// ------------------------------------------------------------------ decoding

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_ne_bytes([b[off], b[off + 1]])
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_ne_bytes(v)
}

/// One request as the kernel wrote it. `len` covers header + payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub offset: u64,
    pub size: u32,
}

impl InHeader {
    /// Parse a request off the front of `buf`; `None` when `buf` is too short
    /// or the header's own length is impossible.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < IN_HEADER_LEN {
            return None;
        }
        let h = InHeader {
            len: u32_at(buf, 0),
            opcode: u32_at(buf, 4),
            unique: u64_at(buf, 8),
            nodeid: u64_at(buf, 16),
            offset: u64_at(buf, 24),
            size: u32_at(buf, 32),
        };
        let len = h.len as usize;
        if len < IN_HEADER_LEN || len > buf.len() || len > READ_BUF_LEN {
            return None;
        }
        Some(h)
    }

    pub fn payload_len(&self) -> usize {
        self.len as usize - IN_HEADER_LEN
    }
}

/// CREATE / MKDIR payload: mode, then the name.
pub fn parse_create_in(payload: &[u8]) -> Option<(u32, &[u8])> {
    if payload.len() < CREATE_IN_LEN {
        return None;
    }
    Some((u32_at(payload, 0), &payload[CREATE_IN_LEN..]))
}

/// RENAME payload: newparent + two name lengths, then both names.
pub fn parse_rename_in(payload: &[u8]) -> Option<(u64, &[u8], &[u8])> {
    if payload.len() < RENAME_IN_LEN {
        return None;
    }
    let newparent = u64_at(payload, 0);
    let namelen = u16_at(payload, 8) as usize;
    let newnamelen = u16_at(payload, 10) as usize;
    let names = &payload[RENAME_IN_LEN..];
    if namelen
        .checked_add(newnamelen)
        .is_none_or(|total| total > names.len())
    {
        return None;
    }
    Some((
        newparent,
        &names[..namelen],
        &names[namelen..namelen + newnamelen],
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SetattrIn {
    pub valid: u32,
    pub mode: u32,
    pub size: u64,
    pub mtime_ns: u64,
}

pub fn parse_setattr_in(payload: &[u8]) -> Option<SetattrIn> {
    if payload.len() < SETATTR_IN_LEN {
        return None;
    }
    Some(SetattrIn {
        valid: u32_at(payload, 0),
        mode: u32_at(payload, 4),
        size: u64_at(payload, 8),
        mtime_ns: u64_at(payload, 16),
    })
}

// ------------------------------------------------------------------ encoding

/// What the kernel wants to hear about one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KernelAttr {
    pub nodeid: u64,
    pub size: u64,
    pub mtime_ns: u64,
    /// S_IF* | permission bits.
    pub mode: u32,
    pub nlink: u32,
}

impl KernelAttr {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.nodeid.to_ne_bytes());
        out.extend_from_slice(&self.size.to_ne_bytes());
        out.extend_from_slice(&self.mtime_ns.to_ne_bytes());
        out.extend_from_slice(&self.mode.to_ne_bytes());
        out.extend_from_slice(&self.nlink.to_ne_bytes());
    }

    pub fn to_bytes(self) -> Vec<u8> {
        let mut v = Vec::with_capacity(ATTR_LEN);
        self.encode(&mut v);
        v
    }

    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
}

pub fn write_out(written: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(WRITE_OUT_LEN);
    v.extend_from_slice(&written.to_ne_bytes());
    v.extend_from_slice(&0u32.to_ne_bytes()); // _pad
    v
}

/// A complete response frame: header (len patched in) followed by `payload`.
/// `error` is 0, a NEGATIVE errno, or — with `unique == 0` — a notify code.
pub fn response(unique: u64, error: i32, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let len = (OUT_HEADER_LEN + payload.len()) as u32;
    let mut v = Vec::with_capacity(len as usize);
    v.extend_from_slice(&len.to_ne_bytes());
    v.extend_from_slice(&error.to_ne_bytes());
    v.extend_from_slice(&unique.to_ne_bytes());
    v.extend_from_slice(payload);
    v
}

/// Round an entry size up to the 8-byte dirent alignment.
pub fn dirent_align(x: usize) -> usize {
    (x + 7) & !7
}

pub fn dirent_size(namelen: usize) -> usize {
    dirent_align(DIRENT_LEN + namelen)
}

/// Append one READDIR entry. `off` is the cursor to resume from AFTER this
/// entry — the daemon owns that numbering; the kernel treats it as opaque.
/// Returns false (appending nothing) when it would not fit in `limit`.
pub fn push_dirent(out: &mut Vec<u8>, limit: usize, nodeid: u64, off: u64, ty: u32, name: &str) -> bool {
    let entry = dirent_size(name.len());
    if name.is_empty() || name.len() > MAX_NAME || out.len() + entry > limit {
        return false;
    }
    out.extend_from_slice(&nodeid.to_ne_bytes());
    out.extend_from_slice(&off.to_ne_bytes());
    out.extend_from_slice(&(name.len() as u32).to_ne_bytes());
    out.extend_from_slice(&ty.to_ne_bytes());
    out.extend_from_slice(name.as_bytes());
    out.resize(out.len() + entry - DIRENT_LEN - name.len(), 0); // pad to align
    true
}

/// One unsolicited notification, ready to write(2): an out header with
/// `unique == 0` and the code in `error`, then a notify entry + its names.
pub struct Notification {
    pub code: i32,
    pub parent: u64,
    pub parent2: u64,
    pub size: u64,
    pub isdir: bool,
    pub name: String,
    pub name2: String,
}

impl Notification {
    pub fn encode(&self) -> Option<Vec<u8>> {
        if self.name.is_empty() || self.name.len() > MAX_NAME || self.name2.len() > MAX_NAME {
            return None;
        }
        let mut payload = Vec::with_capacity(NOTIFY_ENTRY_LEN + self.name.len() + self.name2.len());
        payload.extend_from_slice(&self.parent.to_ne_bytes());
        payload.extend_from_slice(&self.parent2.to_ne_bytes());
        payload.extend_from_slice(&self.size.to_ne_bytes());
        payload.extend_from_slice(&(if self.isdir { NOTIFY_F_ISDIR } else { 0 }).to_ne_bytes());
        payload.extend_from_slice(&(self.name.len() as u16).to_ne_bytes());
        payload.extend_from_slice(&(self.name2.len() as u16).to_ne_bytes());
        payload.extend_from_slice(self.name.as_bytes());
        payload.extend_from_slice(self.name2.as_bytes());
        Some(response(0, self.code, &payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a request the way the kernel would, so the parser is tested
    /// against bytes rather than against itself.
    fn in_header(opcode: u32, unique: u64, nodeid: u64, offset: u64, size: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((IN_HEADER_LEN + payload.len()) as u32).to_ne_bytes());
        v.extend_from_slice(&opcode.to_ne_bytes());
        v.extend_from_slice(&unique.to_ne_bytes());
        v.extend_from_slice(&nodeid.to_ne_bytes());
        v.extend_from_slice(&offset.to_ne_bytes());
        v.extend_from_slice(&size.to_ne_bytes());
        v.extend_from_slice(&0u32.to_ne_bytes()); // _pad
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn header_roundtrip() {
        let buf = in_header(OP_READ, 7, 42, 4096, 128, b"");
        assert_eq!(buf.len(), IN_HEADER_LEN);
        let h = InHeader::parse(&buf).unwrap();
        assert_eq!(h.opcode, OP_READ);
        assert_eq!(h.unique, 7);
        assert_eq!(h.nodeid, 42);
        assert_eq!(h.offset, 4096);
        assert_eq!(h.size, 128);
        assert_eq!(h.payload_len(), 0);
    }

    #[test]
    fn header_with_payload() {
        let buf = in_header(OP_LOOKUP, 1, 1, 0, 0, b"one.txt");
        let h = InHeader::parse(&buf).unwrap();
        assert_eq!(h.payload_len(), 7);
        assert_eq!(&buf[IN_HEADER_LEN..h.len as usize], b"one.txt");
    }

    #[test]
    fn short_and_lying_headers_are_rejected() {
        assert!(InHeader::parse(&[0u8; 8]).is_none());
        let mut buf = in_header(OP_GETATTR, 1, 1, 0, 0, b"");
        // len smaller than the header itself
        buf[0..4].copy_from_slice(&8u32.to_ne_bytes());
        assert!(InHeader::parse(&buf).is_none());
        // len past what was actually read
        buf[0..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(InHeader::parse(&buf).is_none());
    }

    #[test]
    fn create_in_splits_mode_from_name() {
        let mut p = Vec::new();
        p.extend_from_slice(&(S_IFREG | 0o644).to_ne_bytes());
        p.extend_from_slice(&0u32.to_ne_bytes());
        p.extend_from_slice(b"new.txt");
        let (mode, name) = parse_create_in(&p).unwrap();
        assert_eq!(mode, S_IFREG | 0o644);
        assert_eq!(name, b"new.txt");
        assert!(parse_create_in(&p[..4]).is_none());
    }

    #[test]
    fn rename_in_splits_both_names() {
        let mut p = Vec::new();
        p.extend_from_slice(&9u64.to_ne_bytes());
        p.extend_from_slice(&3u16.to_ne_bytes());
        p.extend_from_slice(&5u16.to_ne_bytes());
        p.extend_from_slice(&0u32.to_ne_bytes());
        p.extend_from_slice(b"oldnewer");
        let (newparent, from, to) = parse_rename_in(&p).unwrap();
        assert_eq!(newparent, 9);
        assert_eq!(from, b"old");
        assert_eq!(to, b"newer");
    }

    #[test]
    fn rename_in_rejects_lengths_past_the_payload() {
        let mut p = Vec::new();
        p.extend_from_slice(&9u64.to_ne_bytes());
        p.extend_from_slice(&200u16.to_ne_bytes());
        p.extend_from_slice(&200u16.to_ne_bytes());
        p.extend_from_slice(&0u32.to_ne_bytes());
        p.extend_from_slice(b"short");
        assert!(parse_rename_in(&p).is_none());
    }

    #[test]
    fn setattr_in_decodes() {
        let mut p = Vec::new();
        p.extend_from_slice(&(SETATTR_SIZE | SETATTR_MODE).to_ne_bytes());
        p.extend_from_slice(&0o600u32.to_ne_bytes());
        p.extend_from_slice(&1234u64.to_ne_bytes());
        p.extend_from_slice(&5u64.to_ne_bytes());
        let si = parse_setattr_in(&p).unwrap();
        assert_eq!(si.valid, SETATTR_SIZE | SETATTR_MODE);
        assert_eq!(si.mode, 0o600);
        assert_eq!(si.size, 1234);
        assert_eq!(si.mtime_ns, 5);
    }

    #[test]
    fn response_header_carries_length_and_error() {
        let r = response(11, -ENOENT, b"");
        assert_eq!(r.len(), OUT_HEADER_LEN);
        assert_eq!(u32_at(&r, 0) as usize, OUT_HEADER_LEN);
        assert_eq!(i32::from_ne_bytes([r[4], r[5], r[6], r[7]]), -ENOENT);
        assert_eq!(u64_at(&r, 8), 11);

        let attr = KernelAttr {
            nodeid: 3,
            size: 10,
            mtime_ns: 1,
            mode: S_IFREG | 0o644,
            nlink: 1,
        };
        let r = response(12, 0, &attr.to_bytes());
        assert_eq!(r.len(), OUT_HEADER_LEN + ATTR_LEN);
        assert_eq!(u32_at(&r, 0) as usize, r.len());
        assert_eq!(u64_at(&r, OUT_HEADER_LEN), 3);
    }

    #[test]
    fn dirents_are_eight_byte_aligned_and_bounded() {
        let mut out = Vec::new();
        assert!(push_dirent(&mut out, MAX_PAYLOAD, 2, 3, DT_REG, "a"));
        assert_eq!(out.len(), 32); // 24 + 1, padded to 32
        assert_eq!(out.len() % 8, 0);
        assert!(push_dirent(&mut out, MAX_PAYLOAD, 3, 4, DT_DIR, "abcdefgh"));
        assert_eq!(out.len(), 32 + 32);
        // Name is where the kernel expects it, right after the fixed part.
        assert_eq!(&out[32 + DIRENT_LEN..32 + DIRENT_LEN + 8], b"abcdefgh");
        // A full buffer refuses rather than overflowing.
        let before = out.len();
        let tight = before + 8;
        assert!(!push_dirent(&mut out, tight, 4, 5, DT_REG, "nope"));
        assert_eq!(out.len(), before);
        assert!(!push_dirent(&mut out, MAX_PAYLOAD, 4, 5, DT_REG, ""));
    }

    #[test]
    fn notification_encodes_names_back_to_back() {
        let n = Notification {
            code: NOTIFY_RENAME,
            parent: 1,
            parent2: 2,
            size: 0,
            isdir: true,
            name: "from".into(),
            name2: "to".into(),
        };
        let buf = n.encode().unwrap();
        assert_eq!(u64_at(&buf, 8), 0, "notifications carry unique == 0");
        assert_eq!(
            i32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]),
            NOTIFY_RENAME
        );
        let p = &buf[OUT_HEADER_LEN..];
        assert_eq!(u64_at(p, 0), 1);
        assert_eq!(u64_at(p, 8), 2);
        assert_eq!(u32_at(p, 24), NOTIFY_F_ISDIR);
        assert_eq!(u16_at(p, 28), 4);
        assert_eq!(u16_at(p, 30), 2);
        assert_eq!(&p[NOTIFY_ENTRY_LEN..], b"fromto");
        assert_eq!(buf.len(), u32_at(&buf, 0) as usize);
    }

    #[test]
    fn nameless_notification_is_refused() {
        let n = Notification {
            code: NOTIFY_CREATE,
            parent: 1,
            parent2: 0,
            size: 0,
            isdir: false,
            name: String::new(),
            name2: String::new(),
        };
        assert!(n.encode().is_none(), "the kernel rejects a zero-length name");
    }
}
