#include "cdm-path.h"
#include <cassert>
#include <fstream>
#include <unistd.h>

int main() {
  namespace fs = std::filesystem;
  auto root = fs::temp_directory_path() / ("myrica-cdm-path-" + std::to_string(getpid()));
  fs::remove_all(root);
  auto add = [&](const std::string& path) {
    fs::create_directories((root / path).parent_path());
    std::ofstream(root / path).put('x');
  };
  assert(widevine::discover(root).empty());
  add("shared/widevine/libwidevinecdm.so");
  assert(widevine::discover(root) == "/shared/widevine/libwidevinecdm.so");
  add("var/lib/widevine/libwidevinecdm.so");
  fs::create_directories(root / "var/lib/widevine/WidevineCdm/_platform_specific/linux_arm64");
  fs::create_symlink("../../../libwidevinecdm.so", root / "var/lib/widevine/WidevineCdm/_platform_specific/linux_arm64/libwidevinecdm.so");
  assert(widevine::discover(root) == "/var/lib/widevine/WidevineCdm/_platform_specific/linux_arm64/libwidevinecdm.so");
  const std::string base = "var/lib/widevine/WidevineCdm/";
  const std::string suffix = "/_platform_specific/linux_arm64/libwidevinecdm.so";
  add(base + "4.9.9999.0" + suffix);
  add(base + "4.10.2662.3" + suffix);
  add(base + "invalid" + suffix);
  fs::create_directories(root / (base + "99.0")); // incomplete install
  assert(widevine::discover(root) == "/" + base + "4.10.2662.3" + suffix);
  fs::remove(root / (base + "4.10.2662.3" + suffix));
  assert(widevine::discover(root) == "/" + base + "4.9.9999.0" + suffix);
  fs::remove_all(root);
}
