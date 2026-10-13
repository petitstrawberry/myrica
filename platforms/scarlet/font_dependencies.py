"""Native font libraries for WebKit, built from pinned upstream/fork sources."""
import json
from pathlib import Path
import shutil
import subprocess


REVISIONS = {
    "brotli": "ed738e842d2fbdf2d6459e39267a633c4a9b2f5d",
    "woff2": "1bccf208bca986e53a647dfe4811322adb06ecf8",
    "fontconfig": "65a1ac12a11bc0dc1272d6d36765569c19736a91",
}


def build_font_dependencies(sources, build, prefix, runtime, deps, args, common, cflags, resource, run):
    sources = sources.resolve()
    for name, revision in REVISIONS.items():
        source = sources / name
        actual = subprocess.check_output(["git", "-C", source, "rev-parse", "HEAD"], text=True).strip()
        if actual != revision:
            raise ValueError(f"{name}: expected {revision}, found {actual}")
        if subprocess.check_output(["git", "-C", source, "diff", "HEAD", "--"], text=True):
            raise ValueError(f"{name}: source has uncommitted changes")
    prefix.mkdir(parents=True, exist_ok=True)
    (prefix / "lib/pkgconfig").mkdir(parents=True, exist_ok=True)
    (prefix / "include").mkdir(exist_ok=True)
    cppflags = f'-nostdinc++ -isystem "{runtime}/include/c++/v1" {cflags} -fno-exceptions -fno-rtti'
    options = [f"-DCMAKE_INSTALL_PREFIX={prefix}", f"-DCMAKE_C_FLAGS={cflags}",
               f"-DCMAKE_CXX_FLAGS={cppflags}", "-DBUILD_SHARED_LIBS=OFF",
               f"-DCMAKE_PREFIX_PATH={prefix};{deps};{runtime}",
               f"-DCMAKE_FIND_ROOT_PATH={prefix};{deps};{runtime}",
               f"-DPKG_CONFIG_EXECUTABLE={args.pkg_config}"]
    # Upstream install targets also install command-line tools. The embedding
    # needs only libraries; copy their actual producer outputs and official PCs.
    for name, targets, headers, pcs in (
        ("brotli", ("brotlicommon", "brotlidec", "brotlienc"), "c/include/brotli", ("common", "dec", "enc")),
        ("woff2", ("woff2common", "woff2dec"), "include/woff2", ("common", "dec")),
    ):
        directory = build / name
        run(["cmake", "-S", sources / name, "-B", directory, *common, *options,
             "-DCMAKE_POLICY_VERSION_MINIMUM=3.5"])
        run(["cmake", "--build", directory, "--parallel", args.jobs, "--target", *targets])
        for target in targets:
            shutil.copy2(directory / f"lib{target}.a", prefix / "lib")
        for component in pcs:
            shutil.copy2(directory / f"lib{name}{component}.pc", prefix / "lib/pkgconfig")
        shutil.copytree(sources / name / headers, prefix / "include" / name, dirs_exist_ok=True)

    # Meson discards a compiler's default -isystem path even with -nostdinc.
    # Use -idirafter for Clang's builtin C headers so native checks stay valid.
    cross = build / "fontconfig-cross.ini"
    cross.parent.mkdir(parents=True, exist_ok=True)
    c_args = ["--target=aarch64-unknown-none-elf", "-nostdinc", f"-idirafter{resource}/include",
              f"-isystem{runtime}/include", "-D__scarlet__=1", "-fno-stack-protector", "-mno-outline-atomics"]
    # Meson itself wraps archives in a rescan group; adding a nested group is invalid.
    link_args = ["--target=aarch64-unknown-none-elf", "-nostdlib", "-fuse-ld=lld", f"-L{deps}/lib"]
    link_args += [str(runtime / "lib" / name) for name in
                  ("libscarlet_cxx_support.a", "libscarlet_c_startup.a", "libscarlet_c.a")]
    pc_paths = [str(p / d) for p in (prefix, deps, runtime) for d in ("lib/pkgconfig", "share/pkgconfig")]
    cross.write_text(f"""[binaries]
c = {args.cc!r}
ar = {args.ar!r}
pkg-config = {args.pkg_config!r}
[host_machine]
system = 'scarlet'
cpu_family = 'aarch64'
cpu = 'aarch64'
endian = 'little'
[properties]
needs_exe_wrapper = true
[built-in options]
c_args = {c_args!r}
c_link_args = {link_args!r}
pkg_config_path = {pc_paths!r}
""")
    directory = build / "fontconfig"
    command = [args.meson, "setup", directory, sources / "fontconfig", "--cross-file", cross,
               "--prefix", prefix, "--libdir", "lib", "--buildtype", "release", "--default-library", "static"]
    if (directory / "meson-private/coredata.dat").exists():
        command.append("--wipe")
    command += [f"-D{name}=disabled" for name in ("doc", "nls", "tests", "tools", "cache-build", "iconv")]
    command += ["-Dxml-backend=expat", "-Ddefault-fonts-dirs=/share/fonts", "-Dadditional-fonts-dirs=[]",
                "-Dcache-dir=/tmp/fontconfig-cache", "-Dbaseconfig-dir=/etc/fonts", "-Dconfig-dir=/etc/fonts/conf.d",
                "-Dtemplate-dir=/share/fontconfig/conf.avail", "-Dxml-dir=/share/xml/fontconfig"]
    run(command)
    run([args.meson, "compile", "-C", directory, "-j", args.jobs])
    # Runtime configuration paths belong in the guest bundle, not the host /share.
    run([args.meson, "install", "--tags", "devel", "-C", directory])
    (prefix / "sources.json").write_text(json.dumps(REVISIONS, indent=2) + "\n")
