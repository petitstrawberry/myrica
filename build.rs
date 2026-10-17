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
