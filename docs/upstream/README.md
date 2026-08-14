# Upstream bug reports

Bugs this project found in **other people's** software, written up as
self-contained reports that someone unfamiliar with AlloyFS can act on. Each
one exists because we shipped a workaround, and each links to the exact file
and lines where that workaround lives, so a maintainer can see real code rather
than a paraphrase. Confidence is stated per report and deliberately varies —
one of the three is a symptom with no root cause and says so.

**None of these have been filed, and there is no plan to file them.** That is a
decision, not a backlog item: they are kept here as the engineering record of
*why* three workarounds exist in this codebase. If a workaround ever looks
gratuitous to someone reading the source later, the reasoning is here, with the
reproducer that justified it.

They are nonetheless written to be sendable as-is, so nothing has to be
reconstructed if that decision changes. The right-hand column records how much
each would need before it could go out.

| Report | Where it would go | State if ever sent |
|---|---|---|
| [`winfsp-rs-set-name-nul-length.md`](winfsp-rs-set-name-nul-length.md) | [SnowflakePowered/winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs) issues — ideally as an issue plus a small PR against `src/filesystem/internals/widenameinfo.rs` | **Sendable as-is.** Defect is visible in the crate source; suggested patch included. |
| [`bun-winfsp-enoent.md`](bun-winfsp-enoent.md) | [oven-sh/bun](https://github.com/oven-sh/bun) issues | **Sendable as-is.** Versions recorded from the reporting machine (bun 1.3.14, rclone 1.73.4) and the `memfs-x64.exe` flags checked against its usage output. |
| [`thin-lto-winfsp-ffi-hang.md`](thin-lto-winfsp-ffi-hang.md) | Undecided: rust-lang/rust, winfsp-rs, or the t-compiler Zulip — the report itself lays out how to choose | **Would need work first.** Symptom plus a bisect, no root cause and no reducer. Re-confirm on the current toolchain and attempt the reduction described in the report before filing anywhere. |
