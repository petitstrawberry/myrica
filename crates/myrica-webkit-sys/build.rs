//! WebKit's CMake build exports a response file containing its native link closure.
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn run(command: &mut Command) {
    let status = command.status().expect("start native WebKit build command");
    assert!(status.success(), "native WebKit build failed: {command:?}");
}

fn prepare() -> PathBuf {
    assert_eq!(
        env::var("TARGET").unwrap(),
        "aarch64-unknown-scarlet",
        "the native WebKit producer currently supports AArch64 Scarlet"
    );
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let producer = root.join("platforms/scarlet");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let bootstrap = output.join("sources");
    let runtime = env::var_os("SCARLET_NATIVE_RUNTIME")
        .expect("SCARLET_NATIVE_RUNTIME must identify the installed SDK C/C++ runtime");
    let mut configure = Command::new("cmake");
    configure
        .args(["-S"])
        .arg(&producer)
        .arg("-B")
        .arg(&bootstrap)
        .args(["-G", "Ninja"])
        .arg(format!(
            "-DSCARLET_NATIVE_RUNTIME={}",
            Path::new(&runtime).display()
        ));
    for (cmake_name, env_name, fallback) in [
        ("SCARLET_NATIVE_CC", "SCARLET_NATIVE_CC", "clang"),
        ("SCARLET_NATIVE_CXX", "SCARLET_NATIVE_CXX", "clang++"),
        ("SCARLET_NATIVE_AR", "SCARLET_NATIVE_AR", "llvm-ar"),
        (
            "SCARLET_NATIVE_RANLIB",
            "SCARLET_NATIVE_RANLIB",
            "llvm-ranlib",
        ),
    ] {
        let value = env::var(env_name).unwrap_or_else(|_| fallback.into());
        configure.arg(format!("-D{cmake_name}={value}"));
    }
    run(&mut configure);
    run(Command::new("cmake").arg("--build").arg(&bootstrap).args([
        "--target",
        "scarlet-dependencies",
        "--parallel",
        "4",
    ]));
    let webkit = fs::read_to_string(bootstrap.join("webkit.path")).unwrap();
    let mut engine = Command::new("python3");
    engine
        .arg(producer.join("build-webkit.py"))
        .arg("--webkit-source")
        .arg(webkit)
        .arg("--runtime")
        .arg(runtime)
        .arg("--output")
        .arg(output.join("engine"))
        .arg("--dependencies")
        .arg(bootstrap.join("dependencies/prefix"))
        .arg("--font-sources")
        .arg(bootstrap.join("fonts"));
    for (argument, variable, fallback) in [
        ("--cc", "SCARLET_NATIVE_CC", "clang"),
        ("--cxx", "SCARLET_NATIVE_CXX", "clang++"),
        ("--ar", "SCARLET_NATIVE_AR", "llvm-ar"),
        ("--ranlib", "SCARLET_NATIVE_RANLIB", "llvm-ranlib"),
    ] {
        engine
            .arg(argument)
            .arg(env::var(variable).unwrap_or_else(|_| fallback.into()));
    }
    run(&mut engine);
    output.join("engine/prefix/share/scarlet/ScarletWebView.link")
}

fn main() {
    println!("cargo:rerun-if-env-changed=MYRICA_WEBKIT_LINK_MANIFEST");
    for name in [
        "SCARLET_NATIVE_RUNTIME",
        "SCARLET_NATIVE_CC",
        "SCARLET_NATIVE_CXX",
        "SCARLET_NATIVE_AR",
        "SCARLET_NATIVE_RANLIB",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=../../platforms/scarlet");
    let manifest = env::var_os("MYRICA_WEBKIT_LINK_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(prepare);
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
