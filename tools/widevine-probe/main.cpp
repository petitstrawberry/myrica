// widevine-probe: minimal Widevine CDM host for the Scarlet Linux-view
// bring-up (W0).
//
// Loads a Widevine CDM shared library through Chromium's CDM ABI (interface
// version 10), implements cdm::Host_10, and reports every stage:
//   dlopen -> InitializeCdmModule_4 -> CreateCdmInstance -> Initialize
//   -> CreateSessionAndGenerateRequest -> message (license or provisioning)
//   -> optional UpdateSession with a response supplied by the caller.
//
// Timers are posted on detached threads; Widevine uses them for licence
// expiry, which does not fire during the bring-up window.

#include "api/content_decryption_module.h"

#include <dlfcn.h>
#include <elf.h>
#include <sys/auxv.h>
#include <sys/utsname.h>

#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <iterator>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

namespace {

using ContentDecryptionModule = cdm::ContentDecryptionModule_10;

constexpr const char* kKeySystem = "com.widevine.alpha";

constexpr uint8_t kWidevineSystemId[16] = {
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce,
    0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed};

struct Options {
  std::string cdm_path;
  std::string storage_dir = "widevine-storage";
  std::string dump_dir;
  std::string pssh_hex;
  std::string kid_hex;
  std::string update_hex;
  std::string update_file;
  int timeout_seconds = 15;
  bool persistent = false;
  bool verbose = false;
};

void print_usage() {
  std::cout
      << "usage: widevine-probe [options]" << std::endl
      << "  --cdm PATH          libwidevinecdm.so to load (or WIDEVINE_CDM_PATH)" << std::endl
      << "  --kid-hex HEX       synthesize a Widevine pssh for this key id" << std::endl
      << "  --pssh-hex HEX      cenc init data for CreateSessionAndGenerateRequest" << std::endl
      << "  --update-hex HEX    UpdateSession response payload" << std::endl
      << "  --update-file FILE  UpdateSession response payload from a file" << std::endl
      << "  --persistent        allow_persistent_state=true" << std::endl
      << "  --storage DIR       FileIO storage root (default widevine-storage)" << std::endl
      << "  --dump-dir DIR      write received messages to DIR" << std::endl
      << "  --timeout SECONDS   wait budget per stage (default 15)" << std::endl
      << "  --verbose           log every host callback" << std::endl
      << "exit codes: 0 ok, 1 usage, 2 load, 3 create instance, 4 initialize,"
      << " 5 session, 6 update" << std::endl;
}

bool take_value(int argc, char** argv, int* index, std::string* value) {
  if (*index + 1 >= argc) {
    return false;
  }
  *index += 1;
  *value = argv[*index];
  return true;
}

bool parse_arguments(int argc, char** argv, Options* options) {
  for (int index = 1; index < argc; ++index) {
    std::string argument = argv[index];
    if (argument == "--cdm") {
      if (!take_value(argc, argv, &index, &options->cdm_path)) {
        return false;
      }
    } else if (argument == "--storage") {
      if (!take_value(argc, argv, &index, &options->storage_dir)) {
        return false;
      }
    } else if (argument == "--dump-dir") {
      if (!take_value(argc, argv, &index, &options->dump_dir)) {
        return false;
      }
    } else if (argument == "--pssh-hex") {
      if (!take_value(argc, argv, &index, &options->pssh_hex)) {
        return false;
      }
    } else if (argument == "--kid-hex") {
      if (!take_value(argc, argv, &index, &options->kid_hex)) {
        return false;
      }
    } else if (argument == "--update-hex") {
      if (!take_value(argc, argv, &index, &options->update_hex)) {
        return false;
      }
    } else if (argument == "--update-file") {
      if (!take_value(argc, argv, &index, &options->update_file)) {
        return false;
      }
    } else if (argument == "--timeout") {
      std::string value;
      if (!take_value(argc, argv, &index, &value)) {
        return false;
      }
      options->timeout_seconds = std::atoi(value.c_str());
      if (options->timeout_seconds < 1) {
        return false;
      }
    } else if (argument == "--persistent") {
      options->persistent = true;
    } else if (argument == "--verbose") {
      options->verbose = true;
    } else if (argument == "--help") {
      return false;
    } else {
      std::cout << "unknown argument: " << argument << std::endl;
      return false;
    }
  }
  return true;
}

int hex_digit(char c) {
  if (c >= '0' && c <= '9') {
    return c - 48;
  }
  if (c >= 'a' && c <= 'f') {
    return c - 87;
  }
  if (c >= 'A' && c <= 'F') {
    return c - 55;
  }
  return -1;
}

bool decode_hex(const std::string& text, std::vector<uint8_t>* out) {
  std::string clean;
  for (char c : text) {
    if (c == ' ' || c == ':' || c == ',') {
      continue;
    }
    clean.push_back(c);
  }
  if (clean.empty() || clean.size() % 2 != 0) {
    return false;
  }
  out->clear();
  for (size_t index = 0; index < clean.size(); index += 2) {
    int high = hex_digit(clean[index]);
    int low = hex_digit(clean[index + 1]);
    if (high < 0 || low < 0) {
      return false;
    }
    out->push_back(static_cast<uint8_t>((high << 4) | low));
  }
  return true;
}

std::string encode_hex(const uint8_t* data, size_t size) {
  static const char kDigits[] = "0123456789abcdef";
  std::string out;
  for (size_t index = 0; index < size; ++index) {
    out.push_back(kDigits[data[index] >> 4]);
    out.push_back(kDigits[data[index] & 15]);
  }
  return out;
}

std::string base64_encode(const uint8_t* data, size_t size) {
  static const char kAlphabet[] =
      "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  std::string out;
  size_t index = 0;
  while (index + 3 <= size) {
    uint32_t value = (static_cast<uint32_t>(data[index]) << 16) |
                     (static_cast<uint32_t>(data[index + 1]) << 8) |
                     static_cast<uint32_t>(data[index + 2]);
    out.push_back(kAlphabet[(value >> 18) & 63]);
    out.push_back(kAlphabet[(value >> 12) & 63]);
    out.push_back(kAlphabet[(value >> 6) & 63]);
    out.push_back(kAlphabet[value & 63]);
    index += 3;
  }
  size_t remaining = size - index;
  if (remaining == 1) {
    uint32_t value = static_cast<uint32_t>(data[index]) << 16;
    out.push_back(kAlphabet[(value >> 18) & 63]);
    out.push_back(kAlphabet[(value >> 12) & 63]);
    out.push_back('=');
    out.push_back('=');
  } else if (remaining == 2) {
    uint32_t value = (static_cast<uint32_t>(data[index]) << 16) |
                     (static_cast<uint32_t>(data[index + 1]) << 8);
    out.push_back(kAlphabet[(value >> 18) & 63]);
    out.push_back(kAlphabet[(value >> 12) & 63]);
    out.push_back(kAlphabet[(value >> 6) & 63]);
    out.push_back('=');
  }
  return out;
}

void push_u32_be(std::vector<uint8_t>* out, uint32_t value) {
  out->push_back(static_cast<uint8_t>(value >> 24));
  out->push_back(static_cast<uint8_t>(value >> 16));
  out->push_back(static_cast<uint8_t>(value >> 8));
  out->push_back(static_cast<uint8_t>(value));
}

std::vector<uint8_t> make_widevine_pssh(const std::vector<uint8_t>& key_id) {
  // pssh box carrying a Widevine protobuf payload with one key id.
  std::vector<uint8_t> payload;
  payload.push_back(0x0a);
  payload.push_back(static_cast<uint8_t>(key_id.size()));
  payload.insert(payload.end(), key_id.begin(), key_id.end());
  uint32_t box_size = 4 + 4 + 4 + 16 + 4 + static_cast<uint32_t>(payload.size());
  std::vector<uint8_t> box;
  push_u32_be(&box, box_size);
  box.push_back('p');
  box.push_back('s');
  box.push_back('s');
  box.push_back('h');
  push_u32_be(&box, 0);
  box.insert(box.end(), kWidevineSystemId, kWidevineSystemId + 16);
  push_u32_be(&box, static_cast<uint32_t>(payload.size()));
  box.insert(box.end(), payload.begin(), payload.end());
  return box;
}

std::vector<uint8_t> read_file(const std::string& path) {
  std::ifstream in(path, std::ios::binary);
  return std::vector<uint8_t>(std::istreambuf_iterator<char>(in),
                              std::istreambuf_iterator<char>());
}

void write_file(const std::string& path, const std::vector<uint8_t>& data) {
  std::ofstream out(path, std::ios::binary);
  if (!out) {
    std::cout << "dump failed: " << path << std::endl;
    return;
  }
  out.write(reinterpret_cast<const char*>(data.data()),
            static_cast<std::streamsize>(data.size()));
  std::cout << "dumped: " << path << std::endl;
}

std::string message_type_name(cdm::MessageType type) {
  switch (type) {
    case cdm::kLicenseRequest:
      return "license-request";
    case cdm::kLicenseRenewal:
      return "license-renewal";
    case cdm::kLicenseRelease:
      return "license-release";
    case cdm::kIndividualizationRequest:
      return "individualization-request";
    default:
      return "unknown";
  }
}

std::string key_status_name(cdm::KeyStatus status) {
  switch (status) {
    case cdm::kUsable:
      return "usable";
    case cdm::kInternalError:
      return "internal-error";
    case cdm::kExpired:
      return "expired";
    case cdm::kOutputRestricted:
      return "output-restricted";
    case cdm::kOutputDownscaled:
      return "output-downscaled";
    case cdm::kStatusPending:
      return "status-pending";
    case cdm::kReleased:
      return "released";
    default:
      return "unknown";
  }
}

std::string resolve_cdm_path(const Options& options) {
  if (!options.cdm_path.empty()) {
    return options.cdm_path;
  }
  const char* env = std::getenv("WIDEVINE_CDM_PATH");
  if (env != nullptr && env[0] != 0) {
    return std::string(env);
  }
  // Search the widevine-installer layout for a version directory.
  const std::filesystem::path root("/var/lib/widevine/WidevineCdm");
  std::error_code error;
  if (std::filesystem::is_directory(root, error)) {
    for (const auto& entry : std::filesystem::directory_iterator(root, error)) {
      std::filesystem::path candidate =
          entry.path() / "_platform_specific/linux_arm64/libwidevinecdm.so";
      if (std::filesystem::is_regular_file(candidate, error)) {
        return candidate.string();
      }
    }
  }
  return std::string();
}

void print_environment() {
  struct utsname info;
  if (uname(&info) == 0) {
    std::cout << "uname: " << info.sysname << " " << info.release << " "
              << info.machine << std::endl;
  }
  std::cout << "AT_HWCAP: 0x" << std::hex << getauxval(AT_HWCAP) << std::dec
            << std::endl;
  std::cout << "AT_HWCAP2: 0x" << std::hex << getauxval(AT_HWCAP2) << std::dec
            << std::endl;
}

class ProbeBuffer final : public cdm::Buffer {
 public:
  explicit ProbeBuffer(uint32_t capacity) : data_(capacity) {}
  void Destroy() final { delete this; }
  uint32_t Capacity() const final {
    return static_cast<uint32_t>(data_.size());
  }
  uint8_t* Data() final { return data_.data(); }
  void SetSize(uint32_t size) final { size_ = size; }
  uint32_t Size() const final { return size_; }

 private:
  ~ProbeBuffer() final = default;
  std::vector<uint8_t> data_;
  uint32_t size_ = 0;
};

class ProbeFileIO final : public cdm::FileIO {
 public:
  ProbeFileIO(std::string root, cdm::FileIOClient* client)
      : root_(std::move(root)), client_(client) {}

  void Open(const char* file_name, uint32_t file_name_size) final {
    name_.assign(file_name, file_name + file_name_size);
    if (!valid_name(name_)) {
      std::cout << "[fileio] rejected name: " << name_ << std::endl;
      client_->OnOpenComplete(cdm::FileIOClient::Status::kError);
      return;
    }
    path_ = root_ + "/" + name_;
    opened_ = true;
    std::cout << "[fileio] open " << name_ << " at " << path_ << std::endl;
    client_->OnOpenComplete(cdm::FileIOClient::Status::kSuccess);
  }

  void Read() final {
    if (!opened_) {
      client_->OnReadComplete(cdm::FileIOClient::Status::kError, nullptr, 0);
      return;
    }
    std::vector<uint8_t> data = read_file(path_);
    std::cout << "[fileio] read " << data.size() << " bytes from " << path_
              << std::endl;
    client_->OnReadComplete(cdm::FileIOClient::Status::kSuccess, data.data(),
                            static_cast<uint32_t>(data.size()));
  }

  void Write(const uint8_t* data, uint32_t data_size) final {
    if (!opened_) {
      client_->OnWriteComplete(cdm::FileIOClient::Status::kError);
      return;
    }
    std::vector<uint8_t> payload;
    if (data != nullptr && data_size > 0) {
      payload.assign(data, data + data_size);
    }
    write_file(path_, payload);
    std::cout << "[fileio] wrote " << payload.size() << " bytes to " << path_
              << std::endl;
    client_->OnWriteComplete(cdm::FileIOClient::Status::kSuccess);
  }

  void Close() final { delete this; }

 private:
  ~ProbeFileIO() final = default;

  static bool valid_name(const std::string& name) {
    if (name.empty() || name.size() > 256 || name[0] == '_') {
      return false;
    }
    for (char c : name) {
      bool ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-';
      if (!ok) {
        return false;
      }
    }
    return true;
  }

  std::string root_;
  std::string name_;
  std::string path_;
  cdm::FileIOClient* client_;
  bool opened_ = false;
};

struct Liveness {
  std::mutex mutex;
  ContentDecryptionModule* cdm = nullptr;
  bool destroyed = false;
};

class ProbeHost final : public cdm::Host_10 {
 public:
  ProbeHost(const Options& options, std::shared_ptr<Liveness> live)
      : options_(options), live_(std::move(live)) {}

  cdm::Buffer* Allocate(uint32_t capacity) final {
    if (options_.verbose) {
      std::cout << "[host] Allocate " << capacity << std::endl;
    }
    return new ProbeBuffer(capacity);
  }

  void SetTimer(int64_t delay_ms, void* context) final {
    if (options_.verbose) {
      std::cout << "[host] SetTimer " << delay_ms << " ms" << std::endl;
    }
    auto live = live_;
    std::thread([live, delay_ms, context]() {
      if (delay_ms > 0) {
        std::this_thread::sleep_for(std::chrono::milliseconds(delay_ms));
      }
      std::lock_guard<std::mutex> lock(live->mutex);
      if (!live->destroyed && live->cdm != nullptr) {
        live->cdm->TimerExpired(context);
      }
    }).detach();
  }

  cdm::Time GetCurrentWallTime() final {
    return std::chrono::duration<double>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
  }

  void OnInitialized(bool success) final {
    std::cout << "[host] OnInitialized success="
              << (success ? "true" : "false") << std::endl;
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      initialized_ = true;
      init_success_ = success;
    }
    state_cv_.notify_all();
  }

  void OnResolveKeyStatusPromise(uint32_t promise_id,
                                 cdm::KeyStatus key_status) final {
    std::cout << "[host] OnResolveKeyStatusPromise promise=" << promise_id
              << " status=" << key_status_name(key_status) << std::endl;
  }

  void OnResolveNewSessionPromise(uint32_t promise_id, const char* session_id,
                                  uint32_t session_id_size) final {
    std::string id;
    if (session_id != nullptr && session_id_size > 0) {
      id.assign(session_id, session_id + session_id_size);
    }
    std::cout << "[host] OnResolveNewSessionPromise promise=" << promise_id
              << " session=" << id << std::endl;
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      session_resolved_ = true;
      session_id_ = id;
    }
    state_cv_.notify_all();
  }

  void OnResolvePromise(uint32_t promise_id) final {
    std::cout << "[host] OnResolvePromise promise=" << promise_id << std::endl;
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      resolved_promises_.push_back(promise_id);
    }
    state_cv_.notify_all();
  }

  void OnRejectPromise(uint32_t promise_id, cdm::Exception exception,
                       uint32_t system_code, const char* error_message,
                       uint32_t error_message_size) final {
    std::string text;
    if (error_message != nullptr && error_message_size > 0) {
      text.assign(error_message, error_message + error_message_size);
    }
    std::cout << "[host] OnRejectPromise promise=" << promise_id
              << " exception=" << static_cast<uint32_t>(exception)
              << " system_code=" << system_code << " message=" << text
              << std::endl;
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      rejected_promises_.push_back(promise_id);
    }
    state_cv_.notify_all();
  }

  void OnSessionMessage(const char* session_id, uint32_t session_id_size,
                        cdm::MessageType message_type, const char* message,
                        uint32_t message_size) final {
    std::string id;
    if (session_id != nullptr && session_id_size > 0) {
      id.assign(session_id, session_id + session_id_size);
    }
    std::vector<uint8_t> payload;
    if (message != nullptr && message_size > 0) {
      payload.assign(message, message + message_size);
    }
    std::cout << "[host] OnSessionMessage session=" << id
              << " type=" << message_type_name(message_type)
              << " bytes=" << payload.size() << std::endl;
    std::cout << "MESSAGE_BASE64 " << message_type_name(message_type) << " "
              << base64_encode(payload.data(), payload.size()) << std::endl;
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      message_received_ = true;
      message_type_ = message_type;
      message_ = payload;
      message_session_ = id;
    }
    state_cv_.notify_all();
  }

  void OnSessionKeysChange(const char* session_id, uint32_t session_id_size,
                           bool has_additional_usable_key,
                           const cdm::KeyInformation* keys_info,
                           uint32_t keys_info_count) final {
    std::string id;
    if (session_id != nullptr && session_id_size > 0) {
      id.assign(session_id, session_id + session_id_size);
    }
    std::cout << "[host] OnSessionKeysChange session=" << id
              << " has_additional_usable_key="
              << (has_additional_usable_key ? "true" : "false")
              << " keys=" << keys_info_count << std::endl;
    std::vector<std::string> lines;
    for (uint32_t index = 0; index < keys_info_count; ++index) {
      const cdm::KeyInformation& info = keys_info[index];
      std::string key_id = encode_hex(info.key_id, info.key_id_size);
      std::string line = key_id + " " + key_status_name(info.status);
      std::cout << "[host]   key " << line << std::endl;
      lines.push_back(line);
    }
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      keys_changed_ = true;
      keys_ = lines;
    }
    state_cv_.notify_all();
  }

  void OnExpirationChange(const char* session_id, uint32_t session_id_size,
                          cdm::Time new_expiry_time) final {
    std::string id;
    if (session_id != nullptr && session_id_size > 0) {
      id.assign(session_id, session_id + session_id_size);
    }
    std::cout << "[host] OnExpirationChange session=" << id
              << " expiry=" << new_expiry_time << std::endl;
  }

  void OnSessionClosed(const char* session_id,
                       uint32_t session_id_size) final {
    std::string id;
    if (session_id != nullptr && session_id_size > 0) {
      id.assign(session_id, session_id + session_id_size);
    }
    std::cout << "[host] OnSessionClosed session=" << id << std::endl;
  }

  void SendPlatformChallenge(const char* service_id, uint32_t service_id_size,
                             const char* challenge,
                             uint32_t challenge_size) final {
    (void)challenge;
    std::string id;
    if (service_id != nullptr && service_id_size > 0) {
      id.assign(service_id, service_id + service_id_size);
    }
    std::cout << "[host] SendPlatformChallenge service=" << id
              << " challenge_bytes=" << challenge_size
              << " (not answered by the probe)" << std::endl;
  }

  void EnableOutputProtection(uint32_t desired_protection_mask) final {
    std::cout << "[host] EnableOutputProtection mask="
              << desired_protection_mask << std::endl;
  }

  void QueryOutputProtectionStatus() final {
    std::cout << "[host] QueryOutputProtectionStatus" << std::endl;
    with_cdm([this](ContentDecryptionModule* cdm) {
      cdm->OnQueryOutputProtectionStatus(cdm::kQuerySucceeded,
                                         cdm::kLinkTypeNone,
                                         cdm::kProtectionNone);
    });
  }

  void OnDeferredInitializationDone(cdm::StreamType stream_type,
                                    cdm::Status decoder_status) final {
    std::cout << "[host] OnDeferredInitializationDone stream="
              << static_cast<uint32_t>(stream_type)
              << " status=" << static_cast<uint32_t>(decoder_status)
              << std::endl;
  }

  cdm::FileIO* CreateFileIO(cdm::FileIOClient* client) final {
    if (!options_.persistent) {
      std::cout << "[host] CreateFileIO denied (persistent state disabled)"
                << std::endl;
      return nullptr;
    }
    return new ProbeFileIO(options_.storage_dir, client);
  }

  void RequestStorageId(uint32_t version) final {
    std::cout << "[host] RequestStorageId version=" << version
              << " (responding with an unavailable id)" << std::endl;
    with_cdm([this, version](ContentDecryptionModule* cdm) {
      cdm->OnStorageId(version, nullptr, 0);
    });
  }

  bool wait_initialized() {
    std::unique_lock<std::mutex> lock(state_mutex_);
    return state_cv_.wait_for(lock,
                              std::chrono::seconds(options_.timeout_seconds),
                              [this]() { return initialized_; });
  }

  bool wait_session_resolved() {
    std::unique_lock<std::mutex> lock(state_mutex_);
    return state_cv_.wait_for(
        lock, std::chrono::seconds(options_.timeout_seconds), [this]() {
          return session_resolved_ || !rejected_promises_.empty();
        });
  }

  bool wait_message() {
    std::unique_lock<std::mutex> lock(state_mutex_);
    return state_cv_.wait_for(
        lock, std::chrono::seconds(options_.timeout_seconds),
        [this]() { return message_received_ || !rejected_promises_.empty(); });
  }

  bool wait_promise(uint32_t promise_id) {
    std::unique_lock<std::mutex> lock(state_mutex_);
    return state_cv_.wait_for(
        lock, std::chrono::seconds(options_.timeout_seconds),
        [this, promise_id]() {
          for (uint32_t id : resolved_promises_) {
            if (id == promise_id) {
              return true;
            }
          }
          for (uint32_t id : rejected_promises_) {
            if (id == promise_id) {
              return true;
            }
          }
          return false;
        });
  }

  bool wait_keys_change() {
    std::unique_lock<std::mutex> lock(state_mutex_);
    return state_cv_.wait_for(lock, std::chrono::seconds(options_.timeout_seconds),
                              [this]() { return keys_changed_; });
  }

  bool init_success() {
    std::lock_guard<std::mutex> lock(state_mutex_);
    return init_success_;
  }

  std::string session_id() {
    std::lock_guard<std::mutex> lock(state_mutex_);
    return session_id_;
  }

  cdm::MessageType message_type() {
    std::lock_guard<std::mutex> lock(state_mutex_);
    return message_type_;
  }

  std::vector<uint8_t> message() {
    std::lock_guard<std::mutex> lock(state_mutex_);
    return message_;
  }

  bool promise_rejected(uint32_t promise_id) {
    std::lock_guard<std::mutex> lock(state_mutex_);
    for (uint32_t id : rejected_promises_) {
      if (id == promise_id) {
        return true;
      }
    }
    return false;
  }

  std::vector<std::string> keys() {
    std::lock_guard<std::mutex> lock(state_mutex_);
    return keys_;
  }

 private:
  template <typename Callback>
  void with_cdm(Callback callback) {
    std::lock_guard<std::mutex> lock(live_->mutex);
    if (!live_->destroyed && live_->cdm != nullptr) {
      callback(live_->cdm);
    }
  }

  Options options_;
  std::shared_ptr<Liveness> live_;
  std::mutex state_mutex_;
  std::condition_variable state_cv_;
  bool initialized_ = false;
  bool init_success_ = false;
  bool session_resolved_ = false;
  std::string session_id_;
  std::vector<uint32_t> resolved_promises_;
  std::vector<uint32_t> rejected_promises_;
  bool message_received_ = false;
  cdm::MessageType message_type_ = cdm::kLicenseRequest;
  std::vector<uint8_t> message_;
  std::string message_session_;
  bool keys_changed_ = false;
  std::vector<std::string> keys_;
};

void* get_cdm_host(int host_interface_version, void* user_data) {
  std::cout << "[host] GetCdmHostFunc version=" << host_interface_version
            << std::endl;
  if (host_interface_version > cdm::Host_10::kVersion) {
    std::cout << "[host] unsupported host interface version" << std::endl;
    return nullptr;
  }
  return static_cast<cdm::Host_10*>(static_cast<ProbeHost*>(user_data));
}

}  // namespace

int main(int argc, char** argv) {
  Options options;
  if (!parse_arguments(argc, argv, &options)) {
    print_usage();
    return 1;
  }

  std::string cdm_path = resolve_cdm_path(options);
  if (cdm_path.empty()) {
    std::cout << "A Widevine CDM library path is required (--cdm PATH or "
                 "WIDEVINE_CDM_PATH)."
              << std::endl;
    std::cout << "Known layout: /var/lib/widevine/WidevineCdm/<version>/"
                 "_platform_specific/linux_arm64/libwidevinecdm.so"
              << std::endl;
    return 1;
  }

  std::cout << "PROBE_START" << std::endl;
  print_environment();
  std::cout << "cdm_path: " << cdm_path << std::endl;

  if (options.persistent) {
    std::error_code error;
    std::filesystem::create_directories(options.storage_dir, error);
  }
  if (!options.dump_dir.empty()) {
    std::error_code error;
    std::filesystem::create_directories(options.dump_dir, error);
  }

  void* library = dlopen(cdm_path.c_str(), RTLD_NOW | RTLD_LOCAL);
  if (library == nullptr) {
    std::cout << "PROBE_RESULT dlopen_failed " << dlerror() << std::endl;
    return 2;
  }
  std::cout << "PROBE_STAGE dlopen ok" << std::endl;

  auto initialize_module =
      reinterpret_cast<void (*)()>(dlsym(library, "InitializeCdmModule_4"));
  auto create_instance =
      reinterpret_cast<void* (*)(int, const char*, uint32_t, GetCdmHostFunc,
                                 void*)>(dlsym(library, "CreateCdmInstance"));
  auto get_version =
      reinterpret_cast<const char* (*)()>(dlsym(library, "GetCdmVersion"));
  if (create_instance == nullptr) {
    std::cout << "PROBE_RESULT missing_create_cdm_instance" << std::endl;
    return 2;
  }
  if (initialize_module != nullptr) {
    initialize_module();
    std::cout << "PROBE_STAGE InitializeCdmModule_4 ok" << std::endl;
  } else {
    std::cout << "PROBE_NOTE InitializeCdmModule_4 not exported" << std::endl;
  }
  if (get_version != nullptr) {
    std::cout << "cdm_version: " << get_version() << std::endl;
  }

  auto live = std::make_shared<Liveness>();
  ProbeHost host(options, live);
  void* raw = create_instance(ContentDecryptionModule::kVersion, kKeySystem,
                              static_cast<uint32_t>(std::strlen(kKeySystem)),
                              &get_cdm_host, &host);
  if (raw == nullptr) {
    std::cout << "PROBE_RESULT create_cdm_instance_failed" << std::endl;
    return 3;
  }
  auto* cdm = static_cast<ContentDecryptionModule*>(raw);
  live->cdm = cdm;
  std::cout << "PROBE_STAGE create_cdm_instance ok" << std::endl;

  cdm->Initialize(false, options.persistent, false);
  if (!host.wait_initialized()) {
    std::cout << "PROBE_RESULT initialize_timeout" << std::endl;
    return 4;
  }
  if (!host.init_success()) {
    std::cout << "PROBE_RESULT initialize_rejected" << std::endl;
    return 4;
  }
  std::cout << "PROBE_STAGE initialize ok" << std::endl;

  std::vector<uint8_t> init_data;
  if (!options.pssh_hex.empty()) {
    if (!decode_hex(options.pssh_hex, &init_data)) {
      std::cout << "invalid --pssh-hex" << std::endl;
      return 1;
    }
  } else if (!options.kid_hex.empty()) {
    std::vector<uint8_t> key_id;
    if (!decode_hex(options.kid_hex, &key_id) || key_id.size() != 16) {
      std::cout << "invalid --kid-hex (expects 16 bytes)" << std::endl;
      return 1;
    }
    init_data = make_widevine_pssh(key_id);
  }

  std::string session_id;
  if (init_data.empty()) {
    std::cout << "PROBE_NOTE no init data; stopping after Initialize"
              << std::endl;
  } else {
    std::cout << "init_data_bytes: " << init_data.size() << std::endl;
    cdm->CreateSessionAndGenerateRequest(
        1, cdm::SessionType::kTemporary, cdm::InitDataType::kCenc,
        init_data.data(), static_cast<uint32_t>(init_data.size()));
    if (!host.wait_session_resolved()) {
      std::cout << "PROBE_RESULT session_timeout" << std::endl;
      return 5;
    }
    session_id = host.session_id();
    if (session_id.empty()) {
      std::cout << "PROBE_RESULT session_rejected" << std::endl;
      return 5;
    }
    if (!host.wait_message()) {
      std::cout << "PROBE_RESULT message_timeout" << std::endl;
      return 5;
    }
    if (!host.promise_rejected(1)) {
      std::cout << "PROBE_STAGE session ok" << std::endl;
    } else {
      std::cout << "PROBE_RESULT session_rejected" << std::endl;
      return 5;
    }
  }

  if (!options.update_hex.empty() || !options.update_file.empty()) {
    std::vector<uint8_t> response;
    if (!options.update_file.empty()) {
      response = read_file(options.update_file);
      if (response.empty()) {
        std::cout << "empty --update-file payload" << std::endl;
        return 1;
      }
    } else if (!decode_hex(options.update_hex, &response)) {
      std::cout << "invalid --update-hex" << std::endl;
      return 1;
    }
    if (session_id.empty()) {
      std::cout << "PROBE_RESULT update_without_session" << std::endl;
      return 6;
    }
    std::cout << "update_bytes: " << response.size() << std::endl;
    cdm->UpdateSession(2, session_id.data(),
                       static_cast<uint32_t>(session_id.size()),
                       response.data(),
                       static_cast<uint32_t>(response.size()));
    if (!host.wait_promise(2)) {
      std::cout << "PROBE_RESULT update_timeout" << std::endl;
      return 6;
    }
    if (host.promise_rejected(2)) {
      std::cout << "PROBE_RESULT update_rejected" << std::endl;
      return 6;
    }
    std::cout << "PROBE_STAGE update ok" << std::endl;
    if (host.wait_keys_change()) {
      for (const std::string& key : host.keys()) {
        std::cout << "KEY_STATUS " << key << std::endl;
      }
    }
  }

  if (!options.dump_dir.empty()) {
    std::vector<uint8_t> message = host.message();
    if (!message.empty()) {
      write_file(options.dump_dir + "/message.bin", message);
    }
  }

  {
    std::lock_guard<std::mutex> lock(live->mutex);
    live->destroyed = true;
    cdm->Destroy();
    live->cdm = nullptr;
  }
  std::cout << "PROBE_RESULT ok" << std::endl;
  return 0;
}

