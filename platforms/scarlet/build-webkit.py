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
    parser.add_argument("--webkit-source", required=True, type=Path)
    parser.add_argument("--runtime", type=Path,
                        default=os.environ.get("SCARLET_NATIVE_RUNTIME"),
                        help="Installed scarlet-sdk C/C++ runtime (or SCARLET_NATIVE_RUNTIME)")
    parser.add_argument("--output", required=True, type=Path,
                        help="Caller-owned build directory; Cargo callers use OUT_DIR")
    parser.add_argument("--dependencies", required=True, type=Path)
    parser.add_argument("--font-sources", required=True, type=Path)
    parser.add_argument("--meson", default="meson")
    parser.add_argument("--cc", required=True, help="Raw cross-capable Clang, without a host wrapper")
    parser.add_argument("--cxx", required=True)
    parser.add_argument("--ar", default="llvm-ar")
    parser.add_argument("--ranlib", default="llvm-ranlib")
    parser.add_argument("--pkg-config", default="pkg-config")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--vulkan-probe", action="store_true",
                        help="Build WebKit's native Skia/Vulkan GPU contract probe")
    parser.add_argument("--vulkan-rendering", action="store_true",
                        help="Paint WebCore pages with native Skia/Vulkan")
    parser.add_argument("--media", action="store_true",
                        help="Enable Scarlet H.264/AAC playback and Media Source Extensions")
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    if args.media and not args.vulkan_rendering:
        parser.error("--media requires --vulkan-rendering for GPU video colour conversion")
    for name in ("cc", "cxx", "ar", "ranlib", "pkg_config", "meson"):
        value = getattr(args, name)
        resolved = shutil.which(value)
        if resolved is None:
            parser.error(f"Tool not found: {value}")
        setattr(args, name, resolved)
    webkit = args.webkit_source.resolve()
    if args.runtime is None:
        parser.error("--runtime or SCARLET_NATIVE_RUNTIME is required")
    runtime = args.runtime.resolve()
    for name in ("libscarlet_c.a", "libscarlet_cxx_support.a", "libscarlet_c_startup.a",
                 "libc++.a", "libc++abi.a"):
        if not (runtime / "lib" / name).is_file():
            parser.error(f"SDK runtime library is missing: {runtime / 'lib' / name}")
    deps = args.dependencies.resolve()
    build = args.output.resolve()
    prefix = build / "prefix"
    fonts = build / "fonts-prefix"
    build.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    for key in list(environment):
        if key.startswith(("NIX_CFLAGS", "NIX_LDFLAGS", "NIX_PKG_CONFIG", "PKG_CONFIG_")):
            environment.pop(key)
    environment["PKG_CONFIG_PATH"] = ""
    environment["PKG_CONFIG_LIBDIR"] = ":".join(str(p / d) for p in (fonts, deps, runtime)
                                               for d in ("lib/pkgconfig", "share/pkgconfig"))
    commands = []
    logs = build / "logs"
    logs.mkdir(exist_ok=True)

    def run(command, cwd=None, extra_environment=None):
        command = [str(x) for x in command]
        log = logs / f"{len(commands):02d}-{Path(command[0]).name}.log"
        commands.append({"argv": command, "cwd": str(cwd or Path.cwd()), "log": str(log)})
        (build / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
        with log.open("w") as stream:
            result = subprocess.run(command, cwd=cwd, env=environment | (extra_environment or {}),
                                    stdout=stream, stderr=subprocess.STDOUT)
        if result.returncode:
            lines = log.read_text(errors="replace").splitlines()
            for index, line in enumerate(lines):
                if "error:" in line or "CMake Error" in line:
                    print("\n".join(lines[max(0, index - 1):index + 7]), flush=True)
            print("\n".join(lines[-80:]), flush=True)
            print(f"Full native build log: {log}", flush=True)
            raise subprocess.CalledProcessError(result.returncode, command)

    toolchain = runtime / "share/scarlet/cmake/aarch64-scarlet.cmake"
    if not toolchain.is_file():
        parser.error(f"SDK CMake toolchain is missing: {toolchain}")
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

    build_font_dependencies(args.font_sources, build / "fonts", fonts, runtime, deps,
                            args, common, cflags, resource, run)
    libraries = ";".join(str(runtime / "lib" / name) for name in (
        "libc++.a", "libc++abi.a", "libscarlet_cxx_support.a", "libscarlet_c_startup.a", "libscarlet_c.a"))
    configure_build_install(webkit, build / "webkit", prefix, [
        "-DPORT=Scarlet", "-DSCARLET_BUILD_EMBEDDING=ON", "-DSCARLET_BUILD_RENDERER=OFF",
        "-DENABLE_C_LOOP=OFF",
        "-DENABLE_JIT=ON",
        "-DENABLE_DFG_JIT=ON",
        "-DENABLE_FTL_JIT=ON",
        "-DENABLE_SAMPLING_PROFILER=ON",
        "-DENABLE_WEBASSEMBLY=ON",
        "-DENABLE_WEBASSEMBLY_BBQJIT=OFF",
        "-DENABLE_WEBASSEMBLY_OMGJIT=OFF",
        f"-DSCARLET_BUILD_VULKAN_PROBE={'ON' if args.vulkan_probe else 'OFF'}",
        f"-DSCARLET_USE_SKIA_VULKAN={'ON' if args.vulkan_rendering else 'OFF'}",
        f"-DSCARLET_USE_COORDINATED_COMPOSITOR={'ON' if args.vulkan_rendering else 'OFF'}",
        f"-DSCARLET_USE_NATIVE_MEDIA={'ON' if args.media else 'OFF'}",
        f"-DENABLE_MEDIA_SOURCE={'ON' if args.media else 'OFF'}",
        "-DUSE_THIN_ARCHIVES=OFF", "-DUSE_PCH_CODEGEN=ON", "-DUSE_HEADER_MAPS=ON",
        f"-DCMAKE_C_FLAGS={cflags}",
        f'-DCMAKE_CXX_FLAGS=-nostdinc++ -isystem "{runtime}/include/c++/v1" {cflags} -fno-exceptions -fno-rtti',
        f"-DCMAKE_ASM_COMPILER={args.cc}", f"-DCMAKE_ASM_FLAGS=--target=aarch64-unknown-none-elf {cflags}",
        f"-DCMAKE_FIND_ROOT_PATH={fonts};{runtime};{deps}", f"-DCMAKE_PREFIX_PATH={fonts};{runtime};{deps}",
        f"-DFontconfig_LIBRARY={deps}/lib/libfontconfig.a", f"-DFontconfig_INCLUDE_DIR={deps}/include",
        f"-DSQLite3_LIBRARY={deps}/lib/libsqlite3.a", f"-DSQLite3_INCLUDE_DIR={deps}/include",
        "-DUSE_WOFF2=ON", "-DENABLE_VIDEO=ON",
        f"-DPKG_CONFIG_EXECUTABLE={args.pkg_config}", f"-DSCARLET_NATIVE_LIBRARY_ROOTS={fonts};{runtime};{deps}",
        f"-DSCARLET_CXX_RUNTIME_SUPPORT_LIBRARY={runtime}/lib/libscarlet_cxx_support.a",
        f"-DSCARLET_EMBEDDING_RUNTIME_LIBRARIES={libraries}",
        "-DSCARLET_EMBEDDING_LINK_OPTIONS=--wrap=main;--undefined=__wrap_main",
    ], "ScarletWebView")
    print(prefix / "share/scarlet/ScarletWebView.link")


if __name__ == "__main__":
    main()
