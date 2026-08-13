use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(feature = "system")]
use windows_registry::LOCAL_MACHINE;

static HEADER: &str = r#"
#include <winfsp/winfsp.h>
#include <winfsp/fsctl.h>
#include <winfsp/launch.h>
"#;

#[cfg(not(feature = "system"))]
fn local() -> String {
    let project_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    println!(
        "cargo:rustc-link-search={}",
        project_dir.join("winfsp/lib").to_string_lossy()
    );

    "--include-directory=winfsp/inc".into()
}

#[cfg(feature = "system")]
fn system() -> String {
    if !cfg!(windows) {
        panic!("'system' feature not supported for cross-platform compilation.");
    }

    let directory = LOCAL_MACHINE
        .open("SOFTWARE\\WOW6432Node\\WinFsp")
        .ok()
        .and_then(|u| u.get_string("InstallDir").ok())
        .expect("WinFsp installation directory not found.");

    println!("cargo:rustc-link-search={}/lib", directory);

    format!("--include-directory={}/inc", directory)
}

fn copy_winfsp_dll(winfsp_lib: &str) {
    println!("cargo:rerun-if-env-changed=WINFSP_DLL_OUTPUT_PATH");

    // Get the output path from environment variable
    let dll_out_path = match env::var("WINFSP_DLL_OUTPUT_PATH") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            return;
        }
    };

    if let Err(e) = fs::create_dir_all(&dll_out_path) {
        panic!(
            "Failed to create WinFSP DLL output directory {}: {}",
            dll_out_path.display(),
            e
        );
    }

    let project_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dll_path = project_dir
        .join("winfsp/bin")
        .join(format!("{}.dll", winfsp_lib));
    if !dll_path.exists() {
        panic!(
            "WinFSP DLL source file does not exist: {}",
            dll_path.display()
        );
    }

    let dll_dest = dll_out_path.join(format!("{}.dll", winfsp_lib));
    if let Err(e) = fs::copy(&dll_path, &dll_dest) {
        panic!(
            "Failed to copy {} to {}: {}",
            dll_path.display(),
            dll_dest.display(),
            e
        );
    }
}

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // host needs to be windows
    if cfg!(feature = "docsrs") {
        println!("cargo:warning=WinFSP does not build on any operating system but Windows. This feature is meant for docs.rs only. It will not link when compiled into a binary.");
        File::create(out_dir.join("bindings.rs")).unwrap();
        return;
    }

    // Use the target OS configuration instead of the host OS configuration to enable cross-platform compilation
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_else(|_| "unknown".to_string());
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "unknown".to_string());
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_else(|_| "unknown".to_string());

    if target_os != "windows" {
        panic!("WinFSP is only supported on Windows.");
    }

    #[cfg(feature = "system")]
    let link_include = system();
    #[cfg(not(feature = "system"))]
    let link_include = local();

    // Architecture-specific configuration
    let (winfsp_lib, clang_target) = match (target_arch.as_str(), target_env.as_str()) {
        ("x86_64", "msvc") => ("winfsp-x64", "x86_64-pc-windows-msvc"),
        ("x86", "msvc") => ("winfsp-x86", "x86-pc-windows-msvc"),
        ("aarch64", "msvc") => ("winfsp-a64", "aarch64-pc-windows-msvc"),
        ("x86_64", "gnu") => ("winfsp-x64", "x86_64-pc-windows-gnu"),
        ("x86", "gnu") => ("winfsp-x86", "i686-pc-windows-gnu"),
        ("aarch64", "gnu") => ("winfsp-a64", "aarch64-pc-windows-gnu"),
        _ => panic!("unsupported triple {}", env::var("TARGET").unwrap()),
    };

    println!("cargo:rustc-link-lib=dylib={}", winfsp_lib);
    if target_env == "msvc" {
        println!("cargo:rustc-link-lib=dylib=delayimp");
        println!("cargo:rustc-link-arg=/DELAYLOAD:{}.dll", winfsp_lib);
    } else {
        // GNU-flavoured toolchains (llvm-mingw / *-windows-gnullvm): lld
        // understands --delayload and provides __delayLoadHelper2 itself,
        // so no delayimp import library is needed.
        println!("cargo:rustc-link-arg=-Wl,--delayload={}.dll", winfsp_lib);
    }

    let bindings_path_str = out_dir.join("bindings.rs");

    if !Path::new(&bindings_path_str).exists() {
        let gen_h_path = out_dir.join("gen.h");
        let mut gen_h = File::create(&gen_h_path).expect("could not create file");
        gen_h
            .write_all(HEADER.as_bytes())
            .expect("could not write header file");

        let bindings = bindgen::Builder::default()
            .header(gen_h_path.to_str().unwrap())
            .derive_default(true)
            .blocklist_type("_?P?IMAGE_TLS_DIRECTORY.*")
            .allowlist_function("Fsp.*")
            .allowlist_type("FSP.*")
            .allowlist_type("Fsp.*")
            .allowlist_var("FSP_.*")
            .allowlist_var("Fsp.*")
            .allowlist_var("CTL_CODE")
            .clang_arg("-DUNICODE")
            .clang_arg(link_include);

        let bindings = bindings.clang_arg(&format!("--target={}", clang_target));

        let bindings = bindings
            .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
            .generate()
            .expect("Unable to generate bindings");

        let mut bindings = bindings.to_string();

        // On *-windows-gnu targets clang gives C enums an unsigned underlying
        // type, but the system WinFsp DLL is built with MSVC, where C enums
        // are always `int`. Patch the enum typedef so the generated bindings
        // are identical to the MSVC-target ones (consumers such as the
        // `winfsp` crate rely on the MSVC-shaped bindings).
        if target_env == "gnu" {
            bindings = bindings.replace(
                "pub type FSP_FILE_SYSTEM_OPERATION_GUARD_STRATEGY = ::std::os::raw::c_uint;",
                "pub type FSP_FILE_SYSTEM_OPERATION_GUARD_STRATEGY = ::std::os::raw::c_int;",
            );
        }

        fs::write(out_dir.join("bindings.rs"), bindings).expect("Couldn't write bindings!");
    }

    #[cfg(not(feature = "system"))]
    copy_winfsp_dll(winfsp_lib);
}
