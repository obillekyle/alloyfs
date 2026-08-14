# Upstream bug reports

Bugs this project found in **other people's** software, written up as
self-contained reports that someone unfamiliar with AlloyFS can act on. Each
one exists because we shipped a workaround, and each links to the exact file
and lines where that workaround lives, so a maintainer can see real code rather
than a paraphrase. Nothing here has been filed; these are drafts held until
someone decides to send them. Confidence is stated per report and deliberately
varies — one of the three is a symptom with no root cause and says so.

| Report | Where it goes | Ready to send? |
|---|---|---|
| [`winfsp-rs-set-name-nul-length.md`](winfsp-rs-set-name-nul-length.md) | [SnowflakePowered/winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs) issues — ideally as an issue plus a small PR against `src/filesystem/internals/widenameinfo.rs` | **Yes.** Defect is visible in the crate source; suggested patch included. |
| [`bun-winfsp-enoent.md`](bun-winfsp-enoent.md) | [oven-sh/bun](https://github.com/oven-sh/bun) issues | **Yes.** Versions filled in from the reporting machine (bun 1.3.14, rclone 1.73.4) and the `memfs-x64.exe` flags checked against its usage output. |
| [`thin-lto-winfsp-ffi-hang.md`](thin-lto-winfsp-ffi-hang.md) | Undecided: rust-lang/rust, winfsp-rs, or the t-compiler Zulip — the report itself lays out how to choose | **No.** Symptom plus a bisect, no root cause and no reducer. Re-confirm on the current toolchain and attempt the reduction described in the report before filing anywhere. |
