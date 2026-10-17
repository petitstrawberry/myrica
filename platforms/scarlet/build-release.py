#!/usr/bin/env python3
"""Build the AArch64 desktop executables and a checksum-pinned release bundle."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[2]
ASSET = "myrica-aarch64-unknown-scarlet.tar.gz"


def revision(path):
    return subprocess.check_output(["git", "-C", str(path), "rev-parse", "HEAD"], text=True).strip()


def prepare_project(scarlet, project):
    """Use the normal SDK/SGFX recipes, without building a kernel or other apps."""
    (project / "bsp/.cargo").mkdir(parents=True, exist_ok=True)
    (project / "bsp/.cargo/config.toml").write_text('[build]\ntarget = "aarch64-unknown-none-elf"\n')
    quote = lambda path: json.dumps(str(path))
    (project / "scarlet.toml").write_text(f'''schema_version = 2
[project]
name = "myrica-release"
[bsp]
path = "bsp"
package = "scarlet"
[bsp.kernel]
source = {{ path = {quote(scarlet / 'kernel')} }}
[userspace]
cargo-config = ".scarlet/sgfx/userspace.toml"
[images.runtime]
format = "newc"
output = ".scarlet/images/runtime.cpio"
[[images.runtime.layers]]
kind = "bundle"
path = {quote(scarlet / 'bundles/sgfx-native/bundle.toml')}
[[images.runtime.layers]]
kind = "bundle"
path = {quote(ROOT / 'platforms/scarlet/bundle-source.toml')}
''')


def package(staging, output, source_revision, scarlet_revision, audit):
    output.mkdir(parents=True, exist_ok=True)
    files = {"bin/myrica": staging / "bin/myrica",
             "bin/myrica-vulkan-probe": staging / "bin/myrica-vulkan-probe",
             "share/licenses/myrica/LICENSE": ROOT / "LICENSE"}
    archive = output / ASSET
    with tarfile.open(archive, "w:gz") as tar:
        for name, path in files.items():
            if not path.is_file() or path.is_symlink():
                raise ValueError(f"missing release file: {path}")
            info = tar.gettarinfo(str(path), arcname=name)
            info.uid = info.gid = info.mtime = 0
            info.uname = info.gname = ""
            info.mode = 0o755 if name.startswith("bin/") else 0o644
            with path.open("rb") as stream:
                tar.addfile(info, stream)
    with archive.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    tag = f"scarlet-{source_revision}"
    url = f"https://github.com/petitstrawberry/myrica/releases/download/{tag}/{ASSET}"
    (output / "bundle.toml").write_text(f'''# Myrica {source_revision}; Scarlet build environment {scarlet_revision}
[[layers]]
kind = "archive"
url = "{url}"
sha256 = "{digest}"
format = "tar-gz"
to = "/"
''')
    (output / "SHA256SUMS").write_text(f"{digest}  {ASSET}\n")
    (output / "release.json").write_text(json.dumps({
        "source_revision": source_revision, "scarlet_revision": scarlet_revision,
        "target": "aarch64-unknown-scarlet", "archive": ASSET, "sha256": digest,
        "executables": audit,
    }, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scarlet-source", required=True, type=Path)
    parser.add_argument("--output", type=Path, default=ROOT / "dist/scarlet")
    args = parser.parse_args()
    scarlet = args.scarlet_source.resolve()
    expected = json.loads((ROOT / "platforms/scarlet/release.json").read_text())["scarlet_revision"]
    if revision(scarlet) != expected:
        parser.error(f"Scarlet checkout must be at {expected}")
    project = ROOT / ".release/project"
    prepare_project(scarlet, project)
    # This image contains only userspace. No layer consumes the kernel ELF.
    subprocess.run(["cargo", "scarlet", "image", "--project", str(project),
                    "--image", "runtime", "--release", "--no-build",
                    "--kernel-elf", "/dev/null"], check=True)
    subprocess.run(["git", "-C", str(ROOT), "diff", "--exit-code", "--", "Cargo.lock"], check=True)
    staging = project / ".scarlet/staging/runtime"
    audit = json.loads(subprocess.check_output([
        "python3", str(scarlet / "tools/elf_audit.py"), "--scarlet", "--machine", "aarch64",
        "--require-interpreter", "/bin/scarlet-ld", str(staging / "bin/myrica"),
        str(staging / "bin/myrica-vulkan-probe")], text=True))
    for entry in audit:
        if entry["textrel"] or entry["rpath"] or entry["runpath"]:
            raise ValueError(f"unsupported ELF linkage: {entry['path']}")
        entry["path"] = Path(entry["path"]).name
    package(staging, args.output.resolve(), revision(ROOT), expected, audit)
    shutil.copy2(project / "scarlet.lock", args.output / "scarlet.lock")


if __name__ == "__main__":
    main()
