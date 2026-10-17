"""Check the producer/consumer contract without compiling WebKit."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tarfile
import tempfile
import tomllib
import unittest

spec = importlib.util.spec_from_file_location("scarlet_release", Path(__file__).resolve().parents[1] / "platforms/scarlet/build-release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ScarletReleaseTests(unittest.TestCase):
    def test_archive_installs_both_executables_and_matches_bundle_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            staging, output = root / "staging", root / "release"
            (staging / "bin").mkdir(parents=True)
            for name in ("myrica", "myrica-vulkan-probe"):
                (staging / "bin" / name).write_bytes(name.encode())
            release.package(staging, output, "1" * 40, "2" * 40, [])
            manifest = tomllib.loads((output / "bundle.toml").read_text())
            archive = output / release.ASSET
            self.assertEqual(manifest["layers"][0]["sha256"], hashlib.sha256(archive.read_bytes()).hexdigest())
            with tarfile.open(archive) as tar:
                self.assertEqual(set(tar.getnames()), {"bin/myrica", "bin/myrica-vulkan-probe", "share/licenses/myrica/LICENSE"})
                for name in ("myrica", "myrica-vulkan-probe"):
                    member = tar.getmember("bin/" + name)
                    self.assertEqual(member.mode, 0o755)
                    self.assertEqual(tar.extractfile(member).read(), name.encode())
            metadata = json.loads((output / "release.json").read_text())
            self.assertEqual(metadata["sha256"], manifest["layers"][0]["sha256"])

    def test_project_uses_normal_graph_and_local_source_recipe(self):
        with tempfile.TemporaryDirectory(prefix="myrica release ") as directory:
            project = Path(directory) / "project"
            release.prepare_project(Path(directory) / "Scarlet", project)
            manifest = tomllib.loads((project / "scarlet.toml").read_text())
            layers = manifest["images"]["runtime"]["layers"]
            self.assertTrue(layers[0]["path"].endswith("bundles/sgfx-native/bundle.toml"))
            self.assertTrue(layers[1]["path"].endswith("platforms/scarlet/bundle-source.toml"))
            recipe = tomllib.loads(Path(layers[1]["path"]).read_text())
            self.assertEqual({layer["to"] for layer in recipe["layers"]}, {"/bin/myrica", "/bin/myrica-vulkan-probe"})
            self.assertTrue(all(layer["replace"] for layer in recipe["layers"]))


if __name__ == "__main__":
    unittest.main()
