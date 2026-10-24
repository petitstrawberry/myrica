use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-env-changed=MYRICA_WEBKIT_LINK_MANIFEST");
    if env::var_os("CARGO_FEATURE_BACKEND_WEBKIT").is_none() {
        return;
    }
    let Some(manifest) = env::var_os("DEP_SCARLET_WEBVIEW_LINK_MANIFEST") else {
        return;
    };
    // The native WebKit link inputs include non-PIC C/C++ archives. Rust's
    // Scarlet target otherwise requests PIE, which cannot link those archives.
    println!("cargo:rustc-link-arg=--no-pie");
    // The resident Scarlet interpreter supplies dlopen/dlsym/dlerror. A
    // standalone Myrica build needs the same imports as a bundled build.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("scarlet") {
        // lld only emits executable imports when a DSO is among its inputs.
        // An empty, as-needed DSO enables imports supplied by the interpreter
        // without adding a runtime library dependency.
        let output = std::path::PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
        let archive = output.join("interpreter-imports.a");
        let shared = output.join("interpreter-imports.so");
        fs::write(&archive, b"!<arch>\n").expect("write empty interpreter import archive");
        let status = std::process::Command::new("rust-lld")
            .args(["-flavor", "gnu", "-m", "aarch64elf", "-shared"])
            .arg(&archive)
            .arg("-o")
            .arg(&shared)
            .status()
            .expect("link interpreter import DSO");
        assert!(status.success(), "link interpreter import DSO");
        println!("cargo:rustc-link-arg=--push-state");
        println!("cargo:rustc-link-arg=--as-needed");
        println!("cargo:rustc-link-arg={}", shared.display());
        println!("cargo:rustc-link-arg=--pop-state");
        println!("cargo:rustc-link-arg=--dynamic-linker=/bin/scarlet-ld");
        println!("cargo:rustc-link-arg=--unresolved-symbols=ignore-all");
    }
    println!(
        "cargo:rerun-if-changed={}",
        std::path::Path::new(&manifest).display()
    );
    let contents = fs::read_to_string(manifest).expect("read WebKit link manifest");
    for argument in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .skip(1)
    {
        if !argument.starts_with('-') {
            println!("cargo:rerun-if-changed={argument}");
        }
        println!("cargo:rustc-link-arg={argument}");
    }
}
