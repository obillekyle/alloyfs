# `WideNameInfo::set_name` counts the NUL terminator in the entry's name length

**Target project:** [SnowflakePowered/winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs)
**Type:** bug report (with suggested patch)
**Confidence:** high — the defect is visible in the crate source, the mechanism
is understood, and removing the terminator makes the symptom disappear.

---

## Summary

`WideNameInfo::set_name` appends a NUL terminator to the name **and** includes
that terminator in the size it declares for the entry. WinFsp derives the
entry's `FileNameLength` from that size, so every name written with `set_name`
is handed to the kernel one wide character too long, with a trailing `U+0000`.

The FSD then matches the caller's search pattern against `"file.txt\0"` instead
of `"file.txt"`. Prefix-ish patterns still match, so a bare `dir` looks correct
— but any pattern that is anchored at the end fails. In practice:

```
C:\> dir X:\            ← lists file.txt, looks fine
C:\> dir X:\*.txt       ← "File Not Found"
C:\> del X:\file.txt    ← "Could Not Find X:\file.txt"
```

The same trait is used by `StreamInfo` and `NotifyInfo`, so named streams and
`ReadDirectoryChangesW` notifications carry the same over-long name.

## Affected versions / environment

| | |
|---|---|
| crate | `winfsp` 0.13.0+winfsp-2.1 (crates.io), default features off, `system` + `delayload` + `notify` |
| sys crate | `winfsp-sys` 0.12.1+winfsp-2.1 |
| WinFsp | 2.1 |
| OS | Windows 11 Pro (10.0.26200) |
| target | `x86_64-pc-windows-gnullvm`, llvm-mingw (clang 22.1.8) |

The defect is in pure Rust arithmetic that does not depend on target or
toolchain, so it should reproduce identically on `*-pc-windows-msvc`.

## The code

`src/filesystem/internals/widenameinfo.rs` (0.13.0):

```rust
fn set_name_raw<'a, P: Into<&'a [u16]>>(&mut self, file_name: P) -> Result<()> {
    let file_name = file_name.into();
    if file_name.len() > BUFFER_SIZE {
        return Err(STATUS_INSUFFICIENT_RESOURCES.into());
    }
    self.name_buffer()[0..std::cmp::min(file_name.len(), BUFFER_SIZE)]
        .copy_from_slice(&file_name[0..std::cmp::min(file_name.len(), BUFFER_SIZE)]);
    self.set_size(std::mem::size_of_val(file_name) as u16);   // ← declared length = whole slice
    Ok(())
}

fn set_name<P: AsRef<OsStr>>(&mut self, file_name: P) -> Result<()> {
    let file_name = file_name.as_ref();
    let file_name = file_name
        .encode_wide()
        .chain(iter::once(0))        // ← terminator added …
        .collect::<Vec<_>>();
    self.set_name_raw(file_name.as_slice())   // ← … and then counted
}

fn set_name_cstr<P: AsRef<U16CStr>>(&mut self, file_name: P) -> Result<()> {
    let file_name = file_name.as_ref();
    self.set_name_raw(file_name.as_slice_with_nul())   // ← same problem
}
```

`set_name_raw` treats the length of the slice it is given as the *declared name
length*. `set_name` and `set_name_cstr` both hand it a NUL-terminated slice, so
both over-declare by one `u16`.

`set_size` in `src/filesystem/directory.rs` (and the identical shape in
`notify/notifyinfo.rs` and `filesystem/stream.rs`) writes it into the FFI
struct's `Size` field:

```rust
fn set_size(&mut self, buffer_size: u16) {
    self.size = std::mem::size_of::<DirInfo<0>>() as u16 + buffer_size
}
```

`DirInfo<0>` is layout-checked against `FSP_FSCTL_DIR_INFO`
(`ensure_layout!(FSP_FSCTL_DIR_INFO, DirInfo<0>)`), and WinFsp recovers the
name length as `Size - sizeof(FSP_FSCTL_DIR_INFO)` — i.e. exactly the value
passed to `set_size`. For `"file.txt"` that is 18 bytes (9 wide chars) where it
must be 16 (8 wide chars).

## Reproducer

### A. Unit-level, no mount required

This is the cheapest way to see it. `size` is private, but the first `u16` of
the entry written into the request buffer is the same field, and
`append_to_buffer` is public:

```rust
use winfsp::filesystem::{DirInfo, WideNameInfo};

let mut di = DirInfo::<255>::new();
di.set_name("file.txt").unwrap();

let mut buf = [0u8; 1024];
let mut cursor = 0u32;
assert!(di.append_to_buffer(&mut buf, &mut cursor));

let size = u16::from_le_bytes([buf[0], buf[1]]) as usize;
let name_len_bytes = size - std::mem::size_of::<DirInfo<0>>();

assert_eq!(name_len_bytes, 16); // fails: is 18 — "file.txt" plus a counted NUL
```

> Honesty note: we have **not** executed this snippet. It is derived by reading
> the 0.13.0 source; `append_to_buffer` calls into `FspFileSystemAddDirInfo`, so
> it needs WinFsp present. The arithmetic it asserts (`size_of_val` over a
> 9-element `[u16]` = 18) is checkable by inspection, and an in-crate test
> reading `self.size` directly avoids the FFI call entirely.

### B. End-to-end, the way we hit it

1. Build any winfsp-rs filesystem whose `read_directory` fills entries with
   `DirInfo::set_name`, and mount it on `X:`.
2. Create `X:\file.txt`.
3. `dir X:\` — the file is listed. Looks healthy.
4. `dir X:\*.txt` — **"File Not Found"**.
5. `del X:\file.txt` — **"Could Not Find X:\file.txt"**.
6. Switch the same filesystem to `set_name_raw` with an unterminated
   `encode_utf16()` slice; 4 and 5 start working, with no other change.

> Honesty note: steps 3–6 are what we observed on our own filesystem during
> development (that is where the workaround below came from). We have not re-run
> them against the repository's `memfs` sample — if that sample uses `set_name`
> it should be the smallest self-contained reproducer available to a maintainer.

## Expected vs actual

**Expected:** the entry declares a name length of `2 * name.len_utf16()` bytes,
and the kernel matches the caller's pattern against `"file.txt"`.

**Actual:** the entry declares `2 * (name.len_utf16() + 1)` bytes, the kernel
sees `"file.txt\0"`, and end-anchored patterns never match. Only wildcard-suffix
patterns (`*`) still work, which is why the failure hides behind a bare `dir`
and surfaces in the far more alarming `del`/`*.ext` cases.

## Suggested fix

The narrow fix is to stop counting the terminator. Either:

**(a) Do not write a terminator at all** — WinFsp's `FSP_FSCTL_DIR_INFO`
`FileName` is a counted, unterminated array:

```rust
fn set_name<P: AsRef<OsStr>>(&mut self, file_name: P) -> Result<()> {
    let file_name: Vec<u16> = file_name.as_ref().encode_wide().collect();
    self.set_name_raw(file_name.as_slice())
}

fn set_name_cstr<P: AsRef<U16CStr>>(&mut self, file_name: P) -> Result<()> {
    self.set_name_raw(file_name.as_ref().as_slice())   // was as_slice_with_nul()
}
```

**(b) Or keep writing the terminator but declare the unterminated length** —
separate "how much is copied into the buffer" from "what size is declared".
This preserves the leftover-content behaviour `set_name_raw`'s docs mention,
at the cost of requiring `len + 1 <= BUFFER_SIZE`.

Either way, `set_name_raw`'s documentation should say explicitly that the slice
it receives **is** the name, terminator included if present — that ambiguity is
what makes the two convenience wrappers wrong. Today the doc comment ("If the
input buffer is not null terminated, and the buffer was not reset prior to
setting the name, the previous contents of the buffer will remain…") reads as if
NUL termination were the expected input shape.

We believe `set_name` is the function that should change; `set_name_cstr` has
the same defect by construction, and `set_name_raw` is correct as long as its
contract is stated.

## Workaround in use

We bypass `set_name` entirely and encode without a terminator. Two sites:

- Directory enumeration —
  [`crates/alloyfs-mount-winfsp/src/lib.rs:209-221`](../../crates/alloyfs-mount-winfsp/src/lib.rs)
  (`write_dir_entry`)
- Change notifications —
  [`crates/alloyfs-mount-winfsp/src/lib.rs:672-683`](../../crates/alloyfs-mount-winfsp/src/lib.rs)
  (`emit_one`)

```rust
// Upstream bug workaround: `set_name` appends a NUL terminator AND
// counts it in the entry's name length, which breaks kernel-side
// pattern matching (`del file.txt`, `dir *.txt` → "File Not Found":
// end-anchored patterns never match "file.txt\0"). Encode without the
// terminator and use set_name_raw, whose size is exactly what we pass.
let wide: Vec<u16> = name.encode_utf16().collect();
if dirinfo.set_name_raw(wide.as_slice()).is_err() {
    // Name longer than the 255-unit DirInfo buffer: skip the entry
    // rather than failing the whole enumeration.
    tracing::warn!(name, "skipping directory entry: name too long");
    return Ok(());
}
```

One side effect worth flagging for the fix: with `set_name_raw` the
`> BUFFER_SIZE` check applies to the exact name length, whereas `set_name`'s
terminator silently costs one character of the 255-unit budget. A 255-character
name is representable but `set_name` rejects it.
