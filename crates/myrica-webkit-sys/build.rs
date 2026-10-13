//! WebKit's CMake build exports a response file containing its native link closure.
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=MYRICA_WEBKIT_LINK_MANIFEST");
    let manifest = env::var_os("MYRICA_WEBKIT_LINK_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("../../target/native-webkit/share/scarlet/ScarletWebView.link"));
    let manifest = manifest
        .canonicalize()
        .expect("WebKit link manifest does not exist");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let contents = fs::read_to_string(&manifest).expect("read WebKit link manifest");
    let mut arguments = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let target = arguments
        .next()
        .expect("WebKit link manifest must start with target=<Rust target triple>");
    assert_eq!(
        target,
        format!("target={}", env::var("TARGET").unwrap()),
        "WebKit library and Myrica must use the same target"
    );
    let mut count = 0;
    for argument in arguments {
        assert!(
            !argument.contains(['\n', '\r', '\0']),
            "invalid WebKit linker argument"
        );
        if !argument.starts_with('-') {
            let path = PathBuf::from(argument);
            assert!(
                path.is_absolute() && path.is_file(),
                "WebKit link input must be an existing absolute path: {argument}"
            );
            println!("cargo:rerun-if-changed={argument}");
        }
        count += 1;
    }
    assert!(count > 0, "empty WebKit link closure");
    // Raw linker arguments are not propagated from an rlib to its consumers.
    // The final Myrica executable emits these arguments in its own build script.
    println!("cargo::metadata=link_manifest={}", manifest.display());
}
