#!/usr/bin/env python3
"""Build Myrica's native WebKit libraries with Scarlet's ordinary CMake toolchain."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
from font_dependencies import build_font_dependencies


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scarlet-source", required=True, type=Path)
    parser.add_argument("--webkit-source", required=True, type=Path)
    parser.add_argument("--llvm-source", required=True, type=Path)
    parser.add_argument("--dependencies", required=True, type=Path)
    parser.add_argument("--font-sources", required=True, type=Path)
    parser.add_argument("--sqlite-source", required=True, type=Path)
    parser.add_argument("--meson", default="meson")
    parser.add_argument("--cc", required=True, help="Raw cross-capable Clang, without a host wrapper")
    parser.add_argument("--cxx", required=True)
    parser.add_argument("--ar", default="llvm-ar")
    parser.add_argument("--ranlib", default="llvm-ranlib")
    parser.add_argument("--pkg-config", default="pkg-config")
    parser.add_argument("--jobs", type=int, default=4)
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    for name in ("cc", "cxx", "ar", "ranlib", "pkg_config", "meson"):
        value = getattr(args, name)
        resolved = shutil.which(value)
        if resolved is None:
            parser.error(f"Tool not found: {value}")
        setattr(args, name, resolved)
    myrica = Path(__file__).resolve().parents[2]
    root = args.scarlet_source.resolve()
    webkit = args.webkit_source.resolve()
    llvm = args.llvm_source.resolve()
    deps = args.dependencies.resolve()
    build = myrica / "target/native-webkit-build"
    runtime = myrica / "target/native-webkit-runtime"
    prefix = myrica / "target/native-webkit"
    fonts = myrica / "target/native-webkit-fonts"
    build.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    for key in list(environment):
        if key.startswith(("NIX_CFLAGS", "NIX_LDFLAGS", "NIX_PKG_CONFIG", "PKG_CONFIG_")):
            environment.pop(key)
    environment["PKG_CONFIG_PATH"] = ""
    environment["PKG_CONFIG_LIBDIR"] = ":".join(str(p / d) for p in (fonts, deps, runtime)
                                               for d in ("lib/pkgconfig", "share/pkgconfig"))
    commands = []

    def run(command, cwd=None, extra_environment=None):
        command = [str(x) for x in command]
        commands.append({"argv": command, "cwd": str(cwd or Path.cwd())})
        (build / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
        subprocess.run(command, cwd=cwd, env=environment | (extra_environment or {}), check=True)

    for crate, library in (("scarlet-libc", "libscarlet_c.a"),
                           ("scarlet-webcore-native-memory", "libscarlet_webcore_native_memory.a")):
        package = root / "user/lib" / crate
        run(["cargo", "build", "--manifest-path", package / "Cargo.toml", "--locked",
             "--release", "--target", "aarch64-unknown-scarlet"], root,
            {"CARGO_TARGET_DIR": str(package / "target")})
        (runtime / "lib").mkdir(parents=True, exist_ok=True)
        shutil.copy2(package / "target/aarch64-unknown-scarlet/release" / library, runtime / "lib")

    toolchain = root / "user/lib/scarlet-libc/native/cmake/aarch64-scarlet.cmake"
    resource = subprocess.check_output([args.cc, "-print-resource-dir"], text=True).strip()
    cflags = f'-nostdinc -isystem "{resource}/include" -isystem "{runtime}/include" -D__scarlet__=1 -fno-stack-protector -mno-outline-atomics'
    common = ["-G", "Ninja", f"-DCMAKE_TOOLCHAIN_FILE={toolchain}",
              f"-DCMAKE_C_COMPILER={args.cc}", f"-DCMAKE_CXX_COMPILER={args.cxx}",
              f"-DCMAKE_AR={args.ar}", f"-DCMAKE_RANLIB={args.ranlib}",
              "-DCMAKE_BUILD_TYPE=Release", "-DCMAKE_INSTALL_LIBDIR=lib", "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON"]

    def configure_build_install(source, directory, install, options, target=None):
        run(["cmake", "-S", source, "-B", directory, *common,
             f"-DCMAKE_INSTALL_PREFIX={install}", *options])
        command = ["cmake", "--build", directory, "--parallel", args.jobs]
        if target:
            command += ["--target", target]
        run(command)
        run(["cmake", "--install", directory])

    configure_build_install(root / "user/lib/scarlet-libc/native", build / "c-support", runtime,
                            [f"-DSCARLET_LLVM_LIBC_SOURCE_DIR={llvm}/libc"])
    options = ["-DLLVM_ENABLE_RUNTIMES=libcxx;libcxxabi",
               "-DLLVM_DEFAULT_TARGET_TRIPLE=aarch64-unknown-none-elf",
               "-DLLVM_ENABLE_PER_TARGET_RUNTIME_DIR=OFF", "-DLIBCXXABI_BAREMETAL=ON",
               "-DLIBCXX_ENABLE_FILESYSTEM=ON", "-DLIBCXX_HAS_PTHREAD_API=ON",
               "-DLIBCXXABI_HAS_PTHREAD_API=ON", f"-DCMAKE_C_FLAGS={cflags}",
               f"-DCMAKE_CXX_FLAGS={cflags} -nostdinc++ -D_LIBCPP_HAS_CLOCK_GETTIME"]
    options += [f"-D{name}=OFF" for name in (
        "LLVM_INCLUDE_TESTS", "LLVM_INCLUDE_DOCS", "LIBCXX_INCLUDE_BENCHMARKS",
        "LIBCXX_ENABLE_SHARED", "LIBCXXABI_ENABLE_SHARED", "LIBCXX_ENABLE_EXCEPTIONS",
        "LIBCXXABI_ENABLE_EXCEPTIONS", "LIBCXXABI_USE_LLVM_UNWINDER", "LIBCXXABI_USE_COMPILER_RT",
        "LIBCXX_ENABLE_RANDOM_DEVICE", "LIBCXX_ENABLE_LOCALIZATION", "LIBCXX_ENABLE_WIDE_CHARACTERS",
        "LIBCXX_ENABLE_TIME_ZONE_DATABASE", "LIBCXX_HAS_TERMINAL_AVAILABLE")]
    configure_build_install(llvm / "runtimes", build / "cxx", runtime, options)
    build_font_dependencies(args.font_sources, build / "fonts", fonts, runtime, deps,
                            args, common, cflags, resource, run)
    sqlite_source = args.sqlite_source.resolve()
    sqlite_revision = subprocess.check_output(["git", "-C", sqlite_source, "rev-parse", "HEAD"], text=True).strip()
    if sqlite_revision != "a6a2a07c6537d8745a1d132be084ad84794901bc":
        parser.error("--sqlite-source must use the pinned Scarlet SQLite fork revision")
    sqlite_host = build / "sqlite-host"
    sqlite_host.mkdir(parents=True, exist_ok=True)
    run([sqlite_source / "configure", "--disable-shared", "--disable-readline"], sqlite_host)
    run(["make", "sqlite3.c"], sqlite_host)
    configure_build_install(webkit / "Source/WebCore/scarlet/Dependencies/SQLite", build / "sqlite", fonts,
                            [f"-DSCARLET_SQLITE_SOURCE_DIR={sqlite_host}", f"-DCMAKE_C_FLAGS={cflags}"])
    libraries = ";".join(str(runtime / "lib" / name) for name in (
        "libc++.a", "libc++abi.a", "libscarlet_cxx_support.a", "libscarlet_c_startup.a", "libscarlet_c.a"))
    configure_build_install(webkit, build / "webkit", prefix, [
        "-DPORT=Scarlet", "-DSCARLET_BUILD_EMBEDDING=ON", "-DSCARLET_BUILD_RENDERER=OFF",
        "-DUSE_THIN_ARCHIVES=OFF", "-DUSE_PCH_CODEGEN=ON", "-DUSE_HEADER_MAPS=ON",
        f"-DCMAKE_C_FLAGS={cflags}",
        f'-DCMAKE_CXX_FLAGS=-nostdinc++ -isystem "{runtime}/include/c++/v1" {cflags} -fno-exceptions -fno-rtti',
        f"-DCMAKE_ASM_COMPILER={args.cc}", f"-DCMAKE_ASM_FLAGS=--target=aarch64-unknown-none-elf {cflags}",
        f"-DCMAKE_FIND_ROOT_PATH={fonts};{runtime};{deps}", f"-DCMAKE_PREFIX_PATH={fonts};{runtime};{deps}",
        f"-DFontconfig_LIBRARY={fonts}/lib/libfontconfig.a", f"-DFontconfig_INCLUDE_DIR={fonts}/include",
        f"-DSQLite3_LIBRARY={fonts}/lib/libsqlite3.a", f"-DSQLite3_INCLUDE_DIR={fonts}/include",
        "-DUSE_WOFF2=ON", "-DENABLE_VIDEO=ON",
        f"-DPKG_CONFIG_EXECUTABLE={args.pkg_config}", f"-DSCARLET_NATIVE_LIBRARY_ROOTS={fonts};{runtime};{deps}",
        f"-DSCARLET_NATIVE_MEMORY_LIBRARY={runtime}/lib/libscarlet_webcore_native_memory.a",
        f"-DSCARLET_CXX_RUNTIME_SUPPORT_LIBRARY={runtime}/lib/libscarlet_cxx_support.a",
        f"-DSCARLET_EMBEDDING_RUNTIME_LIBRARIES={libraries}",
        "-DSCARLET_EMBEDDING_LINK_OPTIONS=--wrap=main;--undefined=__wrap_main",
    ], "ScarletWebView")
    print(prefix / "share/scarlet/ScarletWebView.link")


if __name__ == "__main__":
    main()
