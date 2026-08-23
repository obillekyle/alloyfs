fn main() {
    // Same reason alloyfs-cli has this: WinFsp only supports delay-loaded
    // linking of winfsp-x64.dll, which ships in a versioned SxS directory that
    // is not on PATH. Without the delayload flags the import is ordinary, so
    // the loader demands the DLL before `main` runs and the process dies at
    // startup with STATUS_DLL_NOT_FOUND — which is exactly what this crate's
    // test binary did, since a build script on the *consumer* only covers the
    // consumer's own targets.
    //
    // The library itself is linked into a consumer that supplies its own
    // flags, so this exists for the tests. It is harmless either way.
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
