fn main() {
    // WinFsp only supports delay-loaded linking of winfsp-x64.dll; the helper
    // emits the right flags for both MSVC and llvm-mingw (gnullvm) targets.
    // cfg(windows) here is the *host* cfg, which matches the target for the
    // native builds this project does (no cross-compiling to Windows).
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
