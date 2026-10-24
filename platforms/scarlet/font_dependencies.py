"""Native font libraries for WebKit, built from pinned upstream/fork sources."""
import shutil


def build_font_dependencies(sources, build, prefix, runtime, deps, args, common, cflags, resource, run):
    sources = sources.resolve()
    # Nix supplies immutable sources from the package source declarations.
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
