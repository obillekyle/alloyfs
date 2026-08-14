# Release build with `lto = "thin"` hangs on the first read through a WinFsp mount (`gnullvm`/llvm-mingw)

**Target project:** undecided — see "Where this should go" at the bottom.
Candidates: [rust-lang/rust](https://github.com/rust-lang/rust) (A-LTO),
[SnowflakePowered/winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs).
**Type:** request for help narrowing down — **not** a confident miscompilation claim.
**Confidence:** low on cause, high on the correlation. We have a reliable
symptom and a clean one-variable bisect, and **no root cause**. It is entirely
plausible that the bug is latent undefined behaviour in our own FFI glue that
cross-crate inlining merely exposes, rather than a compiler defect. We are
filing this to get help reducing it, and we would like to be told we are wrong.

---

## Summary

Our project is a user-mode filesystem: a Rust binary that implements the WinFsp
callback interface via the `winfsp` crate and presents a network filesystem as a
Windows drive letter.

Built with `lto = false`, release binaries work. Built with `lto = "thin"` and
nothing else changed:

- the drive mounts successfully,
- directory listing appears to work,
- **the first read of file data through the drive hangs**, hard, in a kernel
  wait. The reading process cannot be killed (`taskkill /f` does not return it);
  the machine needs the mount torn down or a reboot to clear it.

Debug builds are fine. `lto = false` release builds are fine.

## Environment

| | |
|---|---|
| host / target | `x86_64-pc-windows-gnullvm` |
| C toolchain | llvm-mingw (clang 22.1.8, `x86_64-w64-windows-gnu`) |
| rustc | 1.97.1 (LLVM 22.1.6) — see honesty note below |
| WinFsp | 2.1 |
| `winfsp` crate | 0.13.0+winfsp-2.1, features `system`, `delayload`, `notify` |
| `winfsp-sys` | 0.12.1+winfsp-2.1, locally vendored with a build-script patch for llvm-mingw/bindgen |
| OS | Windows 11 Pro (10.0.26200) |

> Honesty note on versions: the rustc/clang versions above are what is installed
> on the machine today. The `lto = false` pin has been in our release profile
> since early in the project, so the original observation was made on an earlier
> stable rustc that we did not record. **Before filing this anywhere, re-confirm
> the hang on the current toolchain** — if it no longer reproduces, that is a
> useful result in itself and the report should say so.

## How it was bisected

One variable, toggled from the environment so that nothing else in the build
could differ:

```sh
# hangs on first read through the mount
CARGO_PROFILE_RELEASE_LTO=thin cargo build --release

# works
CARGO_PROFILE_RELEASE_LTO=false cargo build --release
```

No source changes, no feature changes, no dependency changes between the two.
The result was consistent across repeats. That is the entirety of the evidence:
a correlation with the LTO setting. We have **not** bisected across rustc
versions, across `codegen-units`, or across `lto = "fat"`.

## Expected vs actual

**Expected:** `lto = "thin"` changes only optimisation, not observable
behaviour. A release build with thin LTO reads file data through the mount at
the same speed as without.

**Actual:** the first `ReadFile` through the mounted drive never completes. The
calling thread sits in an uninterruptible kernel wait, which is the normal
Windows outcome when a user-mode filesystem's dispatcher never answers the IRP.
That means the observable failure is "the dispatcher thread stopped answering",
which is one step removed from whatever actually went wrong.

## What we can and cannot say about the cause

The honest position is that the evidence is consistent with several very
different explanations and does not distinguish between them.

**Candidate 1 — latent UB in the FFI glue, exposed by cross-crate inlining.**
This is the possibility we would bet on. Our callback layer does several things
that are easy to get subtly wrong, and all of them cross a crate boundary that
thin LTO opens up for inlining:

- WinFsp hands the per-open context back **by shared reference** on every
  callback, from multiple dispatcher threads concurrently; our `FileContext`
  uses `AtomicBool` for the mutable bit and a `DirBuffer` whose mutability is
  interior by design.
- Our callbacks `block_on` a tokio runtime from WinFsp's own dispatcher threads.
  If cross-crate inlining changes when a thread parks, a latent
  ordering/reentrancy assumption could become a deadlock.
- The `delayload` feature means WinFsp entry points go through delay-load
  thunks. We do not know how LTO interacts with those on the mingw-w64
  toolchain, and it is a plausible place for a real difference in generated
  code.
- Panics must not unwind across `extern "C"`. If some path can panic and the
  optimiser reshapes it, behaviour could differ between builds without either
  build being "wrong".

If any of these is the cause, **the bug is ours, not the compiler's**, and
`lto = "thin"` is merely the first configuration that noticed.

**Candidate 2 — a genuine thin-LTO miscompilation.** Possible, but we have no
positive evidence for it: no disassembly, no reduced test case, no confirmation
that the same source is miscompiled in isolation. Nobody should act on this
hypothesis until a reproducer exists.

**Candidate 3 — a toolchain-specific interaction.** `windows-gnullvm` with
llvm-mingw is a less-travelled target. We have not tried the MSVC target at all,
and that single experiment would be the most informative next step: if
`x86_64-pc-windows-msvc` + `lto = "thin"` is fine, the problem is very likely in
the gnullvm/llvm-mingw/delay-load direction rather than in thin LTO as such.

## What a minimal reproducer would need

We do not have one, and this report is not submittable to a compiler tracker
without one. To isolate, a reproducer would have to:

1. **Drop our project entirely.** Start from the `winfsp` crate's own `memfs`
   sample (or a ~200-line filesystem that only implements `open`, `read`,
   `read_directory`, `close`) so no network, tokio, or protocol code is in the
   picture. If thin LTO breaks *that*, the surface shrinks enormously.
2. **Split into at least two crates**, since thin LTO's distinguishing power is
   cross-crate inlining — a single-crate reproducer may not exercise the same
   thing. The natural split is "filesystem impl" and "callback shim".
3. **Remove tokio and `block_on`.** If the hang survives without a runtime, the
   reentrancy hypothesis dies; if it disappears, we have learned where to look.
4. **Test the `delayload` feature both ways.** Static linking versus delay-load
   thunks is a one-line change with very different codegen.
5. **Have a hard timeout in the harness.** The failure mode is an unkillable
   process, so the reproducer must be driven from a separate process that gives
   up after N seconds and reports "hung" rather than hanging the test runner.
6. **Report the toolchain triple explicitly** and be run on both `gnullvm` and
   `msvc`.

Additional diagnostics that would sharpen the report before anyone files it:

- Capture the hung thread's kernel stack (Process Explorer / WinDbg) — is it in
  the FSD waiting for a reply, or is a dispatcher thread deadlocked in
  user mode?
- Enable WinFsp's debug logging and see whether the `Read` request reached user
  mode at all. "Never dispatched" and "dispatched, never answered" point at
  completely different culprits.
- Try `lto = "fat"` and `lto = false, codegen-units = 1`. If fat LTO is fine and
  thin is not, that is a strong signal; if `codegen-units = 1` alone reproduces
  it, LTO is a red herring and it is an inlining/optimisation sensitivity, which
  in turn smells like UB.
- Run the non-LTO build under a thread sanitizer if one is usable on this
  target, or restructure the callback layer to remove the interior mutability
  and see whether the LTO build starts working.

## Workaround in use

The release profile pins LTO off, with the reasoning recorded next to it —
[`Cargo.toml:62-69`](../../Cargo.toml):

```toml
# Final binaries: strip debug symbols. LTO is deliberately OFF: thin-LTO
# miscompiles the WinFsp FFI path (reads through the mounted drive hang hard
# in kernel waits; debug and non-LTO release are fine). Likely latent UB in
# the FFI glue that cross-crate optimization exposes — investigate before
# ever re-enabling.
[profile.release]
lto = false
strip = true
```

(The wording there says "miscompiles", which is stronger than the evidence
supports. If this report is ever filed, that comment should be softened to match
this document.)

The relevant FFI layer, for anyone who wants to look for the UB candidates
listed above, is
[`crates/alloyfs-mount-winfsp/src/lib.rs`](../../crates/alloyfs-mount-winfsp/src/lib.rs)
— in particular `FileContext` (line ~158, atomics + `DirBuffer` interior
mutability), the `WinFspFs` doc comment on `block_on` from dispatcher threads
(line ~171), and `mount` (line ~755).

## Where this should go

**Not ready to send.** As written, this is a symptom plus a bisect, and a
compiler tracker would rightly ask for a reduction first. Recommended order:

1. Re-confirm on the current toolchain (see the versions honesty note).
2. Try `x86_64-pc-windows-msvc` — cheapest high-information experiment.
3. Attempt the memfs-based reduction above.
4. If it reduces and does not involve our code: file at **rust-lang/rust** with
   the `A-LTO` framing.
5. If it only reproduces with the `winfsp` crate in the picture: file at
   **winfsp-rs**, since the shared FFI shim would then be the suspect.
6. If no reduction is achievable, the honest venue is a question on the Rust
   users forum or the t-compiler Zulip, framed as "help me narrow this down",
   rather than a bug report.
