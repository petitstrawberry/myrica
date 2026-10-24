// Linux-view CDM discovery shared by playback and the setup check.
#pragma once
#include <algorithm>
#include <filesystem>
#include <string>
#include <vector>

namespace widevine {
inline std::vector<unsigned long> version(const std::string& text) {
  std::vector<unsigned long> parts;
  unsigned long value = 0;
  bool digit = false;
  for (char c : text + ".") {
    if (c >= '0' && c <= '9') {
      if (value > 100000000) return {};
      value = value * 10 + c - '0';
      digit = true;
    } else if (c == '.' && digit) {
      parts.push_back(value); value = 0; digit = false;
    } else return {};
  }
  return parts;
}
inline std::string discover(const std::filesystem::path& root = "/") {
  namespace fs = std::filesystem;
  auto present = [&](const std::string& path) {
    std::error_code error;
    return fs::is_regular_file(root / path.substr(1), error);
  };
  const std::string base = "/var/lib/widevine/WidevineCdm/";
  std::vector<std::string> versions;
  std::error_code error;
  fs::directory_iterator entries(root / base.substr(1), error);
  for (fs::directory_iterator end; !error && entries != end; entries.increment(error)) {
    auto name = entries->path().filename().string();
    if (!version(name).empty()) versions.push_back(name);
  }
  std::sort(versions.begin(), versions.end(), [](const auto& a, const auto& b) {
    const auto av = version(a), bv = version(b);
    return av == bv ? a > b : av > bv;
  });
  for (const auto& name : versions) {
    auto path = base + name + "/_platform_specific/linux_arm64/libwidevinecdm.so";
    if (present(path)) return path;
  }
  for (const auto* path : {
      "/var/lib/widevine/WidevineCdm/_platform_specific/linux_arm64/libwidevinecdm.so",
      "/usr/lib/chromium/WidevineCdm/_platform_specific/linux_arm64/libwidevinecdm.so",
      "/usr/lib/chromium-browser/WidevineCdm/_platform_specific/linux_arm64/libwidevinecdm.so",
      "/opt/google/chrome/WidevineCdm/_platform_specific/linux_arm64/libwidevinecdm.so",
      "/var/lib/widevine/libwidevinecdm.so",
      "/shared/widevine/libwidevinecdm.so"}) {
    if (present(path)) return path;
  }
  return {};
}
} // namespace widevine
