# `bun install` fails with "ENOENT: Bun could not find a file" on every WinFsp-backed drive

**Target project:** [oven-sh/bun](https://github.com/oven-sh/bun)
**Type:** bug report
**Confidence:** high. Reproduced on three unrelated WinFsp filesystems,
instrumented with Process Monitor, and the identified condition — Mount Manager
registration — flips the failure on and off deterministically. What we have
*not* done is read bun's source to name the exact call site.

---

## Summary

**This is not a bug with any one filesystem. `bun install` fails on
essentially every WinFsp-backed drive on Windows** — rclone mounts, sshfs-win,
WinFsp's own `memfs` sample, and our project — with the same unhelpful error:

```
error: ENOENT: Bun could not find a file
```

The common factor is not the filesystem. It is **how the drive letter was
created**. WinFsp can expose a volume two ways:

- **session-local DOS device** (mountpoint `X:`) — a symbolic link created with
  `DefineDosDevice`, visible only to the creating session. Needs no privileges,
  so it is what almost every WinFsp filesystem does by default, rclone included.
- **Mount Manager registration** (mountpoint `\\.\X:`) — a real, globally
  registered volume mount point. **Requires Administrator.**

bun works on the second and fails on the first. Everything else — same
filesystem, same files, same command — is unchanged.

Because the non-privileged path is the default across the ecosystem, the
practical effect is that bun does not work on WinFsp drives at all unless the
user knows to run their mount elevated. That is why we think this is worth
fixing in bun rather than in each filesystem: one fix in bun covers rclone,
sshfs-win, and everything else.

## Environment

| | |
|---|---|
| OS | Windows 11 Pro (10.0.26200) |
| WinFsp | 2.1 |
| bun | 1.3.14 |
| rclone | 1.73.4 |
| filesystems reproduced on | WinFsp `memfs` sample, rclone `mount`, our own WinFsp filesystem |

## Reproducer

The point of using **WinFsp's own `memfs` sample** is that it removes every
third party from the picture: it ships with WinFsp, it is a few hundred lines,
and it has no network, no cache, and no configuration.

`memfs-x64.exe` lives in WinFsp's `bin` directory (install the Developer /
Samples feature if it is not present). Run it with no arguments to see the exact
flag set for your build; `-m` is the mount point.

**Failing case — session-local drive (no elevation):**

```bat
:: ordinary, non-elevated prompt
memfs-x64.exe -i -F NTFS -n 1000 -s 1000000 -m X:

:: in a second ordinary prompt
mkdir X:\t
cd /d X:\t
echo {"name":"t","dependencies":{"is-odd":"3.0.1"}} > package.json
bun install
:: → error: ENOENT: Bun could not find a file
```

**Working case — Mount Manager drive (elevated):**

```bat
:: Administrator prompt — note the \\.\ prefix
memfs-x64.exe -i -F NTFS -n 1000 -s 1000000 -m \\.\X:

:: same steps as above
bun install
:: → succeeds
```

**Third-party confirmation (identical failure, unrelated codebase):**

```bat
rclone mount myremote: X: --vfs-cache-mode writes
cd /d X:\somedir
bun install
:: → same ENOENT
```

**Controls we ran:** `git` operates normally inside the same session-local
mount — clone, status, checkout, the lot. So the volume is not broken in any
general sense; it is specifically the path-canonicalisation route bun takes that
does not survive it.

> Honesty note on what we did and did not do here: the failures, the Process
> Monitor trace below, and the elevated-mount fix were all observed on Windows
> during development of our own filesystem, and the rclone/memfs cases were run
> to prove it was not our bug. The `memfs-x64.exe` invocation above has since
> been checked against that binary's own usage output on the reporting machine
> — every flag used (`-i`, `-F`, `-n`, `-s`, `-m`) is spelled as memfs documents
> it — and the versions in the table were read off that machine. What has NOT
> been re-run end to end for this write-up is the bun failure itself; it is
> reported from the original observation. The `-m X:` vs `-m \\.\X:` distinction
> is the part that matters and is WinFsp-documented behaviour.

## Expected vs actual

**Expected:** `bun install` behaves the same on a WinFsp drive as on `C:`, as
`git`, `node`, and `npm` do. Failing that, an error that names the path and the
failing operation.

**Actual:** `error: ENOENT: Bun could not find a file` — no path, no syscall, no
indication that anything to do with drive letters is involved. The error is
famous enough to have accumulated its own folklore; we spent a long time
assuming our filesystem was at fault.

## Root cause

Process Monitor, filtered to the bun process, shows the last filesystem
operation before the failure is **`QueryNameInformationFile`** — the kernel
operation behind `GetFinalPathNameByHandle`. Nothing follows it; bun does not
attempt to open, stat, or read anything else. It fails immediately after asking
Windows to canonicalise a path it already holds an open handle to.

The mechanism:

1. `GetFinalPathNameByHandle(..., VOLUME_NAME_DOS)` asks for the path relative
   to the volume, then maps the volume device back to a drive letter.
2. That reverse mapping goes through the Mount Manager's registration of the
   volume.
3. A drive letter created with `DefineDosDevice` is only a symbolic link in the
   **session's** object directory. The underlying volume has no Mount Manager
   registration, so there is no device→drive-letter mapping to find.
4. The DOS-name lookup therefore does not round-trip. bun treats the result as
   "this path does not exist" and surfaces ENOENT.

Registering the volume with the Mount Manager creates the mapping, the lookup
round-trips, and bun proceeds normally. That is the whole of the difference
between our two reproducer cases.

> Confidence split: the *correlation* (Mount Manager registration decides
> success) and the *last observed syscall* (`QueryNameInformationFile`) are
> directly observed. Step 4 — that bun converts a failed final-path lookup into
> ENOENT — is inference from those two facts. We have not read bun's source to
> find the call site, and a maintainer will find it far faster than we would.

## Suggested fix

**Primary:** make canonicalisation failure non-fatal. Where bun calls
`GetFinalPathNameByHandle` (or a wrapper), a failure — or a result that carries
no DOS volume name — should fall back to the path the caller already supplied
rather than becoming ENOENT. The canonical path is an optimisation for
comparison and caching; it is not required to open the file, and the handle bun
is querying is already open and valid.

**Secondary:** whatever the fix, the error deserves the path and the failing
operation in the message. "Bun could not find a file" with no path is the reason
this took a Procmon trace to diagnose.

**Related, separate issue — POSIX-semantics rename.** bun renames its lockfile
with `FILE_RENAME_POSIX_SEMANTICS`. A filesystem that does not advertise POSIX
unlink/rename rejects that with an invalid-parameter status, and bun fails
rather than retrying. We fixed this on our side by declaring the capability
(`supports_posix_unlink_rename(true)` — see
[`crates/alloyfs-mount-winfsp/src/lib.rs:781-785`](../../crates/alloyfs-mount-winfsp/src/lib.rs)),
but rclone and memfs may not, and a fallback to a plain rename on
`STATUS_INVALID_PARAMETER` would make bun work on more of them. Worth splitting
into its own issue if the maintainers prefer.

**Minor, for completeness:** bun appears to probe the reported filesystem name
and to be unhappy with anything that is not NTFS. We report `"NTFS"` for
compatibility (see the comment at
[`crates/alloyfs-mount-winfsp/src/lib.rs:761-765`](../../crates/alloyfs-mount-winfsp/src/lib.rs)).
We did not isolate this one as carefully as the others — treat it as a hint
rather than a claim.

## Workaround in use

We register the volume with the Mount Manager when we can, and warn clearly when
we cannot —
[`crates/alloyfs-mount-winfsp/src/lib.rs:804-830`](../../crates/alloyfs-mount-winfsp/src/lib.rs):

```rust
// Drive letters: register with the Windows Mount Manager when possible
// (mountpoint "\\.\X:"). Session-local DOS-device mounts (plain "X:")
// break GetFinalPathNameByHandle round-trips, which is exactly why bun
// (and friends) die with ENOENT on rclone-style drives. Mount Manager
// registration needs Administrator, so fall back with a warning.
```

For users of any other WinFsp filesystem, the workaround is: **run the mount
elevated and give it a `\\.\X:`-style mount point.** For rclone specifically,
that means an elevated `rclone mount` with the mount-manager-style mountpoint.

This works, but it is a bad deal for users: it makes "run a userspace
filesystem" require Administrator solely to satisfy one package manager.
