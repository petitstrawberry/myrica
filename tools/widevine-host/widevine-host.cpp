// widevine-host: Widevine CDM service process for Scarlet (W1).
//
// Loads a Widevine CDM through Chromium's CDM ABI (interface version 10),
// implements cdm::Host_10, and serves a Scarlet-native client over a UNIX
// socket. Sample data lives in a shared memory region that the client owns.
// The host is the only process that dlopens the CDM, so the native browser
// never loads glibc code.
//
// The host never talks to a license server: it reports license or
// individualization requests to the client and applies the responses the
// client sends back.

#include "api/content_decryption_module.h"
#include "protocol.h"

#include <dlfcn.h>
#include <elf.h>
#include <fcntl.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstdlib>
#include <cerrno>
#include <cstring>
#include <deque>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <string>
#include <thread>
#include <vector>

namespace {

using ContentDecryptionModule = cdm::ContentDecryptionModule_10;

struct Options {
  std::string cdm_path;
  std::string socket_path;
  std::string shm_path;
  std::string storage_dir = "widevine-storage";
  int socket_fd = -1;
  int shm_fd = -1;
  uint32_t slot_count = 4;
  uint32_t slot_size = 1024 * 1024;
  int wait_seconds = 20;
  bool persistent = false;
  bool verbose = false;
};

void log_line(const std::string& text) {
  std::cout << "[host] " << text << std::endl;
}

void append_u32(std::vector<uint8_t>* out, uint32_t value) {
  out->push_back(static_cast<uint8_t>(value));
  out->push_back(static_cast<uint8_t>(value >> 8));
  out->push_back(static_cast<uint8_t>(value >> 16));
  out->push_back(static_cast<uint8_t>(value >> 24));
}

void append_u64(std::vector<uint8_t>* out, uint64_t value) {
  for (int index = 0; index < 8; ++index) {
    out->push_back(static_cast<uint8_t>(value >> (index * 8)));
  }
}

void append_bytes(std::vector<uint8_t>* out, const void* data, size_t size) {
  const uint8_t* bytes = static_cast<const uint8_t*>(data);
  out->insert(out->end(), bytes, bytes + size);
}

void append_string(std::vector<uint8_t>* out, const std::string& value) {
  append_u32(out, static_cast<uint32_t>(value.size()));
  append_bytes(out, value.data(), value.size());
}

std::vector<uint8_t> make_frame(uint32_t kind, const std::vector<uint8_t>& payload) {
  std::vector<uint8_t> frame;
  append_u32(&frame, WVH_MAGIC);
  append_u32(&frame, kind);
  append_u32(&frame, 0);
  append_u32(&frame, static_cast<uint32_t>(payload.size()));
  frame.insert(frame.end(), payload.begin(), payload.end());
  return frame;
}

struct Reader {
  const uint8_t* data = nullptr;
  size_t size = 0;
  size_t position = 0;

  bool u32(uint32_t* value) {
    if (position + 4 > size) {
      return false;
    }
    *value = static_cast<uint32_t>(data[position]) |
             (static_cast<uint32_t>(data[position + 1]) << 8) |
             (static_cast<uint32_t>(data[position + 2]) << 16) |
             (static_cast<uint32_t>(data[position + 3]) << 24);
    position += 4;
    return true;
  }

  bool i32(int32_t* value) {
    uint32_t raw = 0;
    if (!u32(&raw)) {
      return false;
    }
    *value = static_cast<int32_t>(raw);
    return true;
  }

  bool i64(int64_t* value) {
    uint64_t raw = 0;
    for (int index = 0; index < 8; ++index) {
      if (position + 1 > size) {
        return false;
      }
      raw |= static_cast<uint64_t>(data[position]) << (index * 8);
      position += 1;
    }
    *value = static_cast<int64_t>(raw);
    return true;
  }

  bool count_bytes(uint32_t count, const uint8_t** out) {
    if (position + count > size) {
      return false;
    }
    *out = data + position;
    position += count;
    return true;
  }

  bool blob(std::vector<uint8_t>* out) {
    uint32_t count = 0;
    if (!u32(&count)) {
      return false;
    }
    const uint8_t* bytes = nullptr;
    if (!count_bytes(count, &bytes)) {
      return false;
    }
    out->assign(bytes, bytes + count);
    return true;
  }

  bool text(std::string* out) {
    std::vector<uint8_t> bytes;
    if (!blob(&bytes)) {
      return false;
    }
    out->assign(bytes.begin(), bytes.end());
    return true;
  }
};

bool read_full(int fd, void* buffer, size_t size) {
  uint8_t* bytes = static_cast<uint8_t*>(buffer);
  size_t done = 0;
  while (done < size) {
    ssize_t got = ::read(fd, bytes + done, size - done);
    if (got == 0) {
      return false;
    }
    if (got < 0) {
      if (errno == EINTR) {
        continue;
      }
      return false;
    }
    done += static_cast<size_t>(got);
  }
  return true;
}

bool write_full(int fd, const void* buffer, size_t size) {
  const uint8_t* bytes = static_cast<const uint8_t*>(buffer);
  size_t done = 0;
  while (done < size) {
    ssize_t put = ::write(fd, bytes + done, size - done);
    if (put < 0) {
      if (errno == EINTR) {
        continue;
      }
      return false;
    }
    done += static_cast<size_t>(put);
  }
  return true;
}

class SharedRegion {
 public:
  ~SharedRegion() {
    if (base_ != nullptr) {
      munmap(base_, size_);
    }
    if (owned_fd_ && fd_ >= 0) {
      close(fd_);
    }
  }

  bool open_path(const std::string& path, uint32_t slot_count, uint32_t slot_size) {
    const size_t needed = WVH_SHM_HEADER_BYTES + static_cast<size_t>(slot_count) * slot_size;
    fd_ = open(path.c_str(), O_RDWR | O_CREAT, 0600);
    if (fd_ < 0) {
      std::cerr << "open shm failed: " << path << std::endl;
      return false;
    }
    owned_fd_ = true;
    if (ftruncate(fd_, static_cast<off_t>(needed)) != 0) {
      std::cerr << "ftruncate shm failed" << std::endl;
      return false;
    }
    return map(needed, slot_count, slot_size);
  }

  bool map_fd(int fd, uint32_t slot_count, uint32_t slot_size) {
    fd_ = fd;
    owned_fd_ = false;
    const size_t needed = WVH_SHM_HEADER_BYTES + static_cast<size_t>(slot_count) * slot_size;
    // Scarlet's Linux ABI does not report a size for a shared memory descriptor,
    // so the caller-provided layout is authoritative here; a mismatch shows up
    // as a mapping fault. Record whatever the descriptor does report.
    struct stat info;
    if (fstat(fd, &info) != 0) {
      log_line("fstat on the shared memory descriptor failed; using the requested layout");
    } else if (info.st_size > 0 && static_cast<size_t>(info.st_size) < needed) {
      std::cerr << "shm region is smaller than the requested layout" << std::endl;
      return false;
    }
    return map(needed, slot_count, slot_size);
  }

  bool valid() const { return base_ != nullptr; }
  uint8_t* slot(uint32_t index) const {
    return base_ + WVH_SHM_HEADER_BYTES + static_cast<size_t>(index) * slot_size_;
  }

 private:
  bool map(size_t size, uint32_t slot_count, uint32_t slot_size) {
    void* mapped = mmap(nullptr, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd_, 0);
    if (mapped == MAP_FAILED) {
      std::cerr << "mmap shm failed" << std::endl;
      return false;
    }
    base_ = static_cast<uint8_t*>(mapped);
    size_ = size;
    slot_size_ = slot_size;
    wvh_shm_header* header = reinterpret_cast<wvh_shm_header*>(base_);
    header->magic = WVH_MAGIC;
    header->version = WVH_VERSION;
    header->header_bytes = WVH_SHM_HEADER_BYTES;
    header->slot_count = slot_count;
    header->slot_size = slot_size;
    return true;
  }

  uint8_t* base_ = nullptr;
  size_t size_ = 0;
  uint32_t slot_size_ = 0;
  int fd_ = -1;
  bool owned_fd_ = false;
};

class SlotPool {
 public:
  explicit SlotPool(uint32_t count) {
    for (uint32_t index = count; index > 0; --index) {
      free_.push_back(index - 1);
    }
  }

  int take() {
    std::lock_guard<std::mutex> lock(mutex_);
    if (free_.empty()) {
      return -1;
    }
    const int slot = static_cast<int>(free_.back());
    free_.pop_back();
    return slot;
  }

  void release(uint32_t slot) {
    std::lock_guard<std::mutex> lock(mutex_);
    free_.push_back(slot);
  }

 private:
  std::mutex mutex_;
  std::vector<uint32_t> free_;
};

class EventSink {
 public:
  void start(int fd, bool verbose) {
    fd_ = fd;
    verbose_ = verbose;
    running_ = true;
    thread_ = std::thread([this]() { run(); });
  }

  void stop() {
    {
      std::lock_guard<std::mutex> lock(mutex_);
      running_ = false;
      queue_.clear();
    }
    wake_.notify_all();
    if (thread_.joinable()) {
      thread_.join();
    }
  }

  void push(std::vector<uint8_t> frame) {
    {
      std::lock_guard<std::mutex> lock(mutex_);
      if (!running_) {
        return;
      }
      queue_.push_back(std::move(frame));
    }
    wake_.notify_all();
  }

  void push_event(uint32_t kind, const std::vector<uint8_t>& payload) {
    push(make_frame(kind, payload));
  }

 private:
  void run() {
    for (;;) {
      std::vector<uint8_t> frame;
      {
        std::unique_lock<std::mutex> lock(mutex_);
        wake_.wait(lock, [this]() { return !running_ || !queue_.empty(); });
        if (!running_) {
          return;
        }
        frame = std::move(queue_.front());
        queue_.pop_front();
      }
      if (!write_full(fd_, frame.data(), frame.size())) {
        std::cerr << "event write failed" << std::endl;
        return;
      }
    }
  }

  std::mutex mutex_;
  std::condition_variable wake_;
  std::deque<std::vector<uint8_t>> queue_;
  int fd_ = -1;
  bool running_ = false;
  bool verbose_ = false;
  std::thread thread_;
};

class HostBuffer final : public cdm::Buffer {
 public:
  explicit HostBuffer(uint32_t capacity) : data_(capacity) {}
  void Destroy() final { delete this; }
  uint32_t Capacity() const final { return static_cast<uint32_t>(data_.size()); }
  uint8_t* Data() final { return data_.data(); }
  void SetSize(uint32_t size) final { size_ = size; }
  uint32_t Size() const final { return size_; }

 private:
  ~HostBuffer() final = default;
  std::vector<uint8_t> data_;
  uint32_t size_ = 0;
};

class HostFileIO final : public cdm::FileIO {
 public:
  HostFileIO(std::string root, cdm::FileIOClient* client)
      : root_(std::move(root)), client_(client) {}

  void Open(const char* name, uint32_t name_size) final {
    name_.assign(name, name + name_size);
    bool ok = !name_.empty() && name_.size() <= 256 && name_[0] != '_';
    for (char c : name_) {
      const bool allowed = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                           (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-';
      ok = ok && allowed;
    }
    path_ = root_ + "/" + name_;
    opened_ = ok;
    client_->OnOpenComplete(ok ? cdm::FileIOClient::Status::kSuccess
                               : cdm::FileIOClient::Status::kError);
  }

  void Read() final {
    if (!opened_) {
      client_->OnReadComplete(cdm::FileIOClient::Status::kError, nullptr, 0);
      return;
    }
    std::ifstream in(path_, std::ios::binary);
    std::vector<uint8_t> data((std::istreambuf_iterator<char>(in)),
                              std::istreambuf_iterator<char>());
    client_->OnReadComplete(cdm::FileIOClient::Status::kSuccess, data.data(),
                            static_cast<uint32_t>(data.size()));
  }

  void Write(const uint8_t* data, uint32_t size) final {
    if (!opened_) {
      client_->OnWriteComplete(cdm::FileIOClient::Status::kError);
      return;
    }
    std::ofstream out(path_, std::ios::binary | std::ios::trunc);
    if (data != nullptr && size > 0) {
      out.write(reinterpret_cast<const char*>(data), static_cast<std::streamsize>(size));
    }
    client_->OnWriteComplete(out ? cdm::FileIOClient::Status::kSuccess
                                 : cdm::FileIOClient::Status::kError);
  }

  void Close() final { delete this; }

 private:
  ~HostFileIO() final = default;
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

class HostDecryptedBlock final : public cdm::DecryptedBlock {
 public:
  void SetDecryptedBuffer(cdm::Buffer* buffer) final { buffer_ = buffer; }
  cdm::Buffer* DecryptedBuffer() final { return buffer_; }
  void SetTimestamp(int64_t timestamp) final { timestamp_ = timestamp; }
  int64_t Timestamp() const final { return timestamp_; }

 private:
  cdm::Buffer* buffer_ = nullptr;
  int64_t timestamp_ = 0;
};

class CdmHost final : public cdm::Host_10 {
 public:
  CdmHost(const Options& options, SharedRegion& region, SlotPool& slots, EventSink& events)
      : options_(options), region_(region), slots_(slots), events_(events) {}

  void set_liveness(std::shared_ptr<Liveness> live) { live_ = std::move(live); }

  cdm::Buffer* Allocate(uint32_t capacity) final { return new HostBuffer(capacity); }

  void SetTimer(int64_t delay_ms, void* context) final {
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
    log_line(success ? std::string("OnInitialized success=true")
                     : std::string("OnInitialized success=false"));
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      initialized_ = true;
      init_success_ = success;
    }
    state_cv_.notify_all();
  }

  void OnResolveKeyStatusPromise(uint32_t promise_id, cdm::KeyStatus status) final {
    if (options_.verbose) {
      log_line("OnResolveKeyStatusPromise promise=" + std::to_string(promise_id) +
               " status=" + std::to_string(static_cast<uint32_t>(status)));
    }
  }

  void OnResolveNewSessionPromise(uint32_t promise_id, const char* session_id,
                                  uint32_t session_id_size) final {
    std::string id;
    if (session_id != nullptr && session_id_size > 0) {
      id.assign(session_id, session_id + session_id_size);
    }
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      sessions_[promise_id] = id;
    }
    std::vector<uint8_t> payload;
    append_u32(&payload, promise_id);
    append_string(&payload, id);
    events_.push_event(WVH_EV_SESSION, payload);
    state_cv_.notify_all();
  }

  void OnResolvePromise(uint32_t promise_id) final {
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      resolved_.insert(promise_id);
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
    log_line("OnRejectPromise promise=" + std::to_string(promise_id) +
             " exception=" + std::to_string(static_cast<uint32_t>(exception)) +
             " system_code=" + std::to_string(system_code) + " message=" + text);
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      rejected_[promise_id] = text.empty() ? WVH_ERROR : WVH_ERROR;
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
    const int slot = slots_.take();
    if (slot < 0) {
      log_line("no free slot for session message");
      return;
    }
    if (message_size > options_.slot_size || (message_size && !message)) {
      slots_.release(static_cast<uint32_t>(slot));
      log_line("invalid session message size");
      return;
    }
    if (message != nullptr && message_size > 0) {
      memcpy(region_.slot(static_cast<uint32_t>(slot)), message, message_size);
    }
    log_line("OnSessionMessage type=" + std::to_string(static_cast<uint32_t>(message_type)) +
             " bytes=" + std::to_string(message_size) + " slot=" + std::to_string(slot));
    std::vector<uint8_t> payload;
    append_string(&payload, id);
    append_u32(&payload, static_cast<uint32_t>(message_type));
    append_u32(&payload, static_cast<uint32_t>(slot));
    append_u32(&payload, 0);
    append_u32(&payload, message_size);
    events_.push_event(WVH_EV_MESSAGE, payload);
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      message_sessions_.insert(id);
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
    log_line("OnSessionKeysChange keys=" + std::to_string(keys_info_count) +
             " usable=" + std::string(has_additional_usable_key ? "true" : "false"));
    std::vector<uint8_t> payload;
    append_string(&payload, id);
    append_u32(&payload, keys_info_count);
    for (uint32_t index = 0; index < keys_info_count; ++index) {
      const cdm::KeyInformation& info = keys_info[index];
      if (info.status != cdm::kUsable && info.status != cdm::kStatusPending) {
        log_line("key error status=" +
                 std::to_string(static_cast<uint32_t>(info.status)) +
                 " system_code=" + std::to_string(info.system_code));
      }
      append_u32(&payload, info.key_id_size);
      append_bytes(&payload, info.key_id, info.key_id_size);
      append_u32(&payload, static_cast<uint32_t>(info.status));
    }
    events_.push_event(WVH_EV_KEYS, payload);
  }

  void OnExpirationChange(const char* session_id, uint32_t session_id_size,
                          cdm::Time new_expiry_time) final {
    std::string id;
    if (session_id != nullptr && session_id_size > 0) {
      id.assign(session_id, session_id + session_id_size);
    }
    std::vector<uint8_t> payload;
    append_string(&payload, id);
    uint64_t expiry_bits;
    static_assert(sizeof(expiry_bits) == sizeof(new_expiry_time));
    std::memcpy(&expiry_bits, &new_expiry_time, sizeof(expiry_bits));
    append_u64(&payload, expiry_bits);
    events_.push_event(WVH_EV_EXPIRATION, payload);
  }

  void OnSessionClosed(const char* session_id, uint32_t session_id_size) final {
    std::string id;
    if (session_id != nullptr && session_id_size > 0) {
      id.assign(session_id, session_id + session_id_size);
    }
    std::vector<uint8_t> payload;
    append_string(&payload, id);
    events_.push_event(WVH_EV_SESSION_CLOSED, payload);
  }

  void SendPlatformChallenge(const char* service_id, uint32_t service_id_size,
                             const char* challenge, uint32_t challenge_size) final {
    (void)challenge;
    std::string id;
    if (service_id != nullptr && service_id_size > 0) {
      id.assign(service_id, service_id + service_id_size);
    }
    log_line("SendPlatformChallenge service=" + id +
             " bytes=" + std::to_string(challenge_size));
  }

  void EnableOutputProtection(uint32_t mask) final {
    if (options_.verbose) {
      log_line("EnableOutputProtection mask=" + std::to_string(mask));
    }
  }

  void QueryOutputProtectionStatus() final {
    with_cdm([](ContentDecryptionModule* cdm) {
      cdm->OnQueryOutputProtectionStatus(cdm::kQuerySucceeded, cdm::kLinkTypeNone,
                                         cdm::kProtectionNone);
    });
  }

  void OnDeferredInitializationDone(cdm::StreamType stream_type,
                                    cdm::Status status) final {
    log_line("OnDeferredInitializationDone stream=" +
             std::to_string(static_cast<uint32_t>(stream_type)) +
             " status=" + std::to_string(static_cast<uint32_t>(status)));
  }

  cdm::FileIO* CreateFileIO(cdm::FileIOClient* client) final {
    if (!options_.persistent) {
      return nullptr;
    }
    return new HostFileIO(options_.storage_dir, client);
  }

  void RequestStorageId(uint32_t version) final {
    with_cdm([version](ContentDecryptionModule* cdm) {
      cdm->OnStorageId(version, nullptr, 0);
    });
  }

  bool wait_init() {
    std::unique_lock<std::mutex> lock(state_mutex_);
    return state_cv_.wait_for(lock, std::chrono::seconds(options_.wait_seconds),
                              [this]() { return initialized_; });
  }

  bool wait_session(uint32_t request_id, std::string* session_id) {
    std::unique_lock<std::mutex> lock(state_mutex_);
    const bool ready = state_cv_.wait_for(
        lock, std::chrono::seconds(options_.wait_seconds), [this, request_id]() {
          auto session = sessions_.find(request_id);
          return (session != sessions_.end() && message_sessions_.count(session->second))
              || rejected_.count(request_id) != 0;
        });
    if (!ready || rejected_.count(request_id) != 0) {
      return false;
    }
    *session_id = sessions_[request_id];
    return true;
  }

  bool wait_promise(uint32_t request_id) {
    std::unique_lock<std::mutex> lock(state_mutex_);
    return state_cv_.wait_for(lock, std::chrono::seconds(options_.wait_seconds),
                              [this, request_id]() {
                                return resolved_.count(request_id) != 0 ||
                                       rejected_.count(request_id) != 0;
                              }) &&
           rejected_.count(request_id) == 0;
  }

  bool init_success() {
    std::lock_guard<std::mutex> lock(state_mutex_);
    return init_success_;
  }

  void set_cdm(ContentDecryptionModule* cdm) { cdm_ = cdm; }

  ContentDecryptionModule* cdm() const { return cdm_; }

 private:
  template <typename Callback>
  void with_cdm(Callback callback) {
    std::lock_guard<std::mutex> lock(live_->mutex);
    if (!live_->destroyed && live_->cdm != nullptr) {
      callback(live_->cdm);
    }
  }

  Options options_;
  SharedRegion& region_;
  SlotPool& slots_;
  EventSink& events_;
  std::shared_ptr<Liveness> live_;
  ContentDecryptionModule* cdm_ = nullptr;
  std::mutex state_mutex_;
  std::condition_variable state_cv_;
  bool initialized_ = false;
  bool init_success_ = false;
  std::map<uint32_t, std::string> sessions_;
  std::set<uint32_t> resolved_;
  std::set<std::string> message_sessions_;
  std::map<uint32_t, std::string> rejected_;
};

void push_result(EventSink& events, uint32_t request_id, int32_t status, uint32_t value0,
                 uint32_t value1) {
  std::vector<uint8_t> payload;
  append_u32(&payload, request_id);
  append_u32(&payload, static_cast<uint32_t>(status));
  append_u32(&payload, value0);
  append_u32(&payload, value1);
  events.push_event(WVH_EV_RESULT, payload);
}

bool parse_arguments(int argc, char** argv, Options* options) {
  for (int index = 1; index < argc; ++index) {
    const std::string argument = argv[index];
    const bool needs_value = argument == "--cdm" || argument == "--socket" ||
                             argument == "--fd" || argument == "--shm" ||
                             argument == "--shm-fd" || argument == "--storage" ||
                             argument == "--slot-size" || argument == "--slot-count";
    std::string value;
    if (needs_value) {
      if (index + 1 >= argc) {
        return false;
      }
      value = argv[++index];
    }
    if (argument == "--cdm") {
      options->cdm_path = value;
    } else if (argument == "--socket") {
      options->socket_path = value;
    } else if (argument == "--fd") {
      options->socket_fd = std::atoi(value.c_str());
    } else if (argument == "--shm") {
      options->shm_path = value;
    } else if (argument == "--shm-fd") {
      options->shm_fd = std::atoi(value.c_str());
    } else if (argument == "--storage") {
      options->storage_dir = value;
    } else if (argument == "--slot-size") {
      options->slot_size = static_cast<uint32_t>(std::atoi(value.c_str()));
    } else if (argument == "--slot-count") {
      options->slot_count = static_cast<uint32_t>(std::atoi(value.c_str()));
    } else if (argument == "--persistent") {
      options->persistent = true;
    } else if (argument == "--verbose") {
      options->verbose = true;
    } else {
      std::cerr << "unknown argument: " << argument << std::endl;
      return false;
    }
  }
  if (options->cdm_path.empty() || options->slot_count == 0 || options->slot_size == 0) {
    return false;
  }
  if (options->socket_path.empty() && options->socket_fd < 0) {
    return false;
  }
  if (options->shm_path.empty() && options->shm_fd < 0) {
    return false;
  }
  return true;
}

int open_control_socket(const Options& options) {
  if (options.socket_fd >= 0) {
    return options.socket_fd;
  }
  const int listener = socket(AF_UNIX, SOCK_STREAM, 0);
  if (listener < 0) {
    std::cerr << "socket failed" << std::endl;
    return -1;
  }
  sockaddr_un address;
  memset(&address, 0, sizeof(address));
  address.sun_family = AF_UNIX;
  if (options.socket_path.size() >= sizeof(address.sun_path)) {
    std::cerr << "socket path too long" << std::endl;
    return -1;
  }
  memcpy(address.sun_path, options.socket_path.c_str(), options.socket_path.size());
  unlink(options.socket_path.c_str());
  if (bind(listener, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0) {
    std::cerr << "bind failed" << std::endl;
    return -1;
  }
  if (listen(listener, 1) != 0) {
    std::cerr << "listen failed" << std::endl;
    return -1;
  }
  log_line("listening on " + options.socket_path);
  const int client = accept(listener, nullptr, nullptr);
  if (client < 0) {
    std::cerr << "accept failed" << std::endl;
    return -1;
  }
  close(listener);
  return client;
}

void* get_cdm_host(int host_interface_version, void* user_data) {
  if (host_interface_version > cdm::Host_10::kVersion) {
    std::cerr << "unsupported host interface version" << std::endl;
    return nullptr;
  }
  return static_cast<cdm::Host_10*>(static_cast<CdmHost*>(user_data));
}

class HostVideoFrame final : public cdm::VideoFrame {
 public:
  void SetFormat(cdm::VideoFormat format) final { format_ = format; }
  cdm::VideoFormat Format() const final { return format_; }
  void SetSize(cdm::Size size) final { size_ = size; }
  cdm::Size Size() const final { return size_; }
  void SetFrameBuffer(cdm::Buffer* buffer) final { buffer_ = buffer; }
  cdm::Buffer* FrameBuffer() final { return buffer_; }
  void SetPlaneOffset(cdm::VideoPlane plane, uint32_t offset) final {
    if (plane < cdm::kMaxPlanes) {
      offsets_[plane] = offset;
    }
  }
  uint32_t PlaneOffset(cdm::VideoPlane plane) final {
    return plane < cdm::kMaxPlanes ? offsets_[plane] : 0;
  }
  void SetStride(cdm::VideoPlane plane, uint32_t stride) final {
    if (plane < cdm::kMaxPlanes) {
      strides_[plane] = stride;
    }
  }
  uint32_t Stride(cdm::VideoPlane plane) final {
    return plane < cdm::kMaxPlanes ? strides_[plane] : 0;
  }
  void SetTimestamp(int64_t timestamp) final { timestamp_ = timestamp; }
  int64_t Timestamp() const final { return timestamp_; }

 private:
  cdm::VideoFormat format_ = cdm::kUnknownVideoFormat;
  cdm::Size size_ = {0, 0};
  cdm::Buffer* buffer_ = nullptr;
  uint32_t offsets_[cdm::kMaxPlanes] = {0, 0, 0};
  uint32_t strides_[cdm::kMaxPlanes] = {0, 0, 0};
  int64_t timestamp_ = 0;
};

struct ParsedInput {
  uint32_t request_id = 0;
  uint32_t slot = 0;
  uint32_t offset = 0;
  uint32_t size = 0;
  std::vector<uint8_t> key_id;
  std::vector<uint8_t> iv;
  std::vector<cdm::SubsampleEntry> subsamples;
  uint32_t scheme = 0;
  int64_t timestamp = 0;
};

std::string to_hex(const std::vector<uint8_t>& bytes) {
  static const char* digits = "0123456789abcdef";
  std::string out;
  for (uint8_t byte : bytes) {
    out.push_back(digits[byte >> 4]);
    out.push_back(digits[byte & 15]);
  }
  return out;
}

bool parse_input(const std::vector<uint8_t>& payload, ParsedInput* parsed) {
  Reader reader{payload.data(), payload.size(), 0};
  uint32_t count = 0;
  if (!reader.u32(&parsed->request_id) || !reader.u32(&parsed->slot) ||
      !reader.u32(&parsed->offset) || !reader.u32(&parsed->size)) {
    return false;
  }
  if (!reader.blob(&parsed->key_id) || !reader.blob(&parsed->iv) || !reader.u32(&count)) {
    return false;
  }
  for (uint32_t index = 0; index < count; ++index) {
    uint32_t clear_bytes = 0;
    uint32_t cipher_bytes = 0;
    if (!reader.u32(&clear_bytes) || !reader.u32(&cipher_bytes)) {
      return false;
    }
    cdm::SubsampleEntry entry;
    entry.clear_bytes = clear_bytes;
    entry.cipher_bytes = cipher_bytes;
    parsed->subsamples.push_back(entry);
  }
  return reader.u32(&parsed->scheme) && reader.i64(&parsed->timestamp);
}

void fill_input_buffer(const ParsedInput& parsed, SharedRegion& region, cdm::InputBuffer_2* input) {
  memset(input, 0, sizeof(*input));
  input->data = region.slot(parsed.slot) + parsed.offset;
  input->data_size = parsed.size;
  if (parsed.scheme == 0) {
    input->encryption_scheme = cdm::EncryptionScheme::kUnencrypted;
  } else if (parsed.scheme == WVH_SCHEME_CBCS) {
    input->encryption_scheme = cdm::EncryptionScheme::kCbcs;
  } else {
    input->encryption_scheme = cdm::EncryptionScheme::kCenc;
  }
  input->key_id = parsed.key_id.empty() ? nullptr : parsed.key_id.data();
  input->key_id_size = static_cast<uint32_t>(parsed.key_id.size());
  input->iv = parsed.iv.empty() ? nullptr : parsed.iv.data();
  input->iv_size = static_cast<uint32_t>(parsed.iv.size());
  input->subsamples = parsed.subsamples.empty() ? nullptr : parsed.subsamples.data();
  input->num_subsamples = static_cast<uint32_t>(parsed.subsamples.size());
  input->timestamp = parsed.timestamp;
}

bool push_frame(EventSink& events, uint32_t request_id, HostVideoFrame& frame, SharedRegion& region,
                SlotPool& slots, uint32_t slot_size, uint32_t* out_slot, uint32_t* out_size) {
  cdm::Buffer* buffer = frame.FrameBuffer();
  if (buffer == nullptr) {
    return false;
  }
  const int slot = slots.take();
  const uint32_t size = buffer->Size();
  if (slot < 0 || size > slot_size) {
    return false;
  }
  memcpy(region.slot(static_cast<uint32_t>(slot)), buffer->Data(), size);
  const cdm::Size frame_size = frame.Size();
  std::vector<uint8_t> payload;
  append_u32(&payload, request_id);
  append_u32(&payload, static_cast<uint32_t>(frame.Format()));
  append_u32(&payload, static_cast<uint32_t>(frame_size.width));
  append_u32(&payload, static_cast<uint32_t>(frame_size.height));
  append_u32(&payload, static_cast<uint32_t>(slot));
  append_u32(&payload, size);
  append_u32(&payload, frame.Stride(cdm::kYPlane));
  append_u32(&payload, frame.Stride(cdm::kUPlane));
  append_u32(&payload, frame.Stride(cdm::kVPlane));
  append_u32(&payload, frame.PlaneOffset(cdm::kUPlane));
  append_u32(&payload, frame.PlaneOffset(cdm::kVPlane));
  append_u64(&payload, static_cast<uint64_t>(frame.Timestamp()));
  events.push_event(WVH_EV_FRAME, payload);
  *out_slot = static_cast<uint32_t>(slot);
  *out_size = size;
  return true;
}

void handle_decrypt(const Options& options, CdmHost& host, EventSink& events,
                    SharedRegion& region, SlotPool& slots,
                    const std::vector<uint8_t>& payload) {
  ParsedInput parsed;
  if (!parse_input(payload, &parsed)) {
    push_result(events, 0, WVH_ERROR, 0, 0);
    return;
  }
  if (options.verbose) {
    log_line("decrypt request key_id=" + to_hex(parsed.key_id) + " iv=" + to_hex(parsed.iv) +
             " scheme=" + std::to_string(parsed.scheme) +
             " bytes=" + std::to_string(parsed.size) +
             " subsamples=" + std::to_string(parsed.subsamples.size()));
  }
  cdm::InputBuffer_2 input;
  fill_input_buffer(parsed, region, &input);
  HostDecryptedBlock block;
  const cdm::Status status = host.cdm()->Decrypt(input, &block);
  if (options.verbose) {
    log_line("decrypt status=" + std::to_string(static_cast<uint32_t>(status)));
  }
  if (status == cdm::kSuccess && block.DecryptedBuffer() != nullptr) {
    const int out_slot = slots.take();
    const uint32_t out_size = block.DecryptedBuffer()->Size();
    if (out_slot >= 0 && out_size <= options.slot_size) {
      memcpy(region.slot(static_cast<uint32_t>(out_slot)), block.DecryptedBuffer()->Data(),
             out_size);
      push_result(events, parsed.request_id, WVH_OK, static_cast<uint32_t>(out_slot), out_size);
    } else {
      push_result(events, parsed.request_id, WVH_ERROR, 0, 0);
    }
  } else {
    push_result(events, parsed.request_id,
                status == cdm::kNoKey ? WVH_NO_KEY : WVH_DECRYPT_ERROR, 0, 0);
  }
  if (block.DecryptedBuffer() != nullptr) {
    block.DecryptedBuffer()->Destroy();
  }
}

void handle_decode(const Options& options, CdmHost& host, EventSink& events,
                   SharedRegion& region, SlotPool& slots,
                   const std::vector<uint8_t>& payload) {
  ParsedInput parsed;
  if (!parse_input(payload, &parsed)) {
    push_result(events, 0, WVH_ERROR, 0, 0);
    return;
  }
  if (options.verbose) {
    log_line("decode request key_id=" + to_hex(parsed.key_id) + " iv=" + to_hex(parsed.iv) +
             " bytes=" + std::to_string(parsed.size) +
             " subsamples=" + std::to_string(parsed.subsamples.size()));
  }
  cdm::InputBuffer_2 input;
  fill_input_buffer(parsed, region, &input);
  HostVideoFrame frame;
  const cdm::Status status = host.cdm()->DecryptAndDecodeFrame(input, &frame);
  uint32_t out_slot = 0;
  uint32_t out_size = 0;
  if (status == cdm::kSuccess) {
    push_frame(events, parsed.request_id, frame, region, slots, options.slot_size, &out_slot,
               &out_size);
  }
  if (frame.FrameBuffer() != nullptr) {
    frame.FrameBuffer()->Destroy();
  }
  if (options.verbose) {
    log_line("decode status=" + std::to_string(static_cast<uint32_t>(status)) +
             " frame_slot=" + std::to_string(out_slot) + " frame_size=" + std::to_string(out_size));
  }
  push_result(events, parsed.request_id,
              status == cdm::kSuccess ? WVH_OK
                                      : (status == cdm::kNoKey ? WVH_NO_KEY : WVH_DECRYPT_ERROR),
              out_slot, out_size);
}

void handle_configure_video(const Options& options, CdmHost& host, EventSink& events,
                            const std::vector<uint8_t>& payload) {
  Reader reader{payload.data(), payload.size(), 0};
  wvh_video_config config;
  std::vector<uint8_t> extra_data;
  if (!reader.u32(&config.codec) || !reader.u32(&config.profile) || !reader.u32(&config.format) ||
      !reader.u32(&config.coded_width) || !reader.u32(&config.coded_height) ||
      !reader.blob(&extra_data)) {
    push_result(events, 0, WVH_ERROR, 0, 0);
    return;
  }
  cdm::VideoDecoderConfig_2 decoder_config;
  memset(&decoder_config, 0, sizeof(decoder_config));
  decoder_config.codec = static_cast<cdm::VideoCodec>(config.codec);
  decoder_config.profile = static_cast<cdm::VideoCodecProfile>(config.profile);
  decoder_config.format = static_cast<cdm::VideoFormat>(config.format);
  decoder_config.coded_size.width = static_cast<int32_t>(config.coded_width);
  decoder_config.coded_size.height = static_cast<int32_t>(config.coded_height);
  decoder_config.extra_data = extra_data.empty() ? nullptr : extra_data.data();
  decoder_config.extra_data_size = static_cast<uint32_t>(extra_data.size());
  decoder_config.encryption_scheme = cdm::EncryptionScheme::kCenc;
  const cdm::Status status = host.cdm()->InitializeVideoDecoder(decoder_config);
  if (options.verbose) {
    log_line("configure video codec=" + std::to_string(config.codec) +
             " profile=" + std::to_string(config.profile) + " size=" +
             std::to_string(config.coded_width) + "x" + std::to_string(config.coded_height) +
             " extra=" + std::to_string(extra_data.size()) +
             " status=" + std::to_string(static_cast<uint32_t>(status)));
  }
  push_result(events, 0, status == cdm::kSuccess ? WVH_OK : WVH_ERROR, 0, 0);
}

void serve(int client_fd, const Options& options, CdmHost& host, EventSink& events,
           SharedRegion& region, SlotPool& slots, ContentDecryptionModule* cdm) {
  bool running = true;
  while (running) {
    uint8_t header[16];
    if (!read_full(client_fd, header, sizeof(header))) {
      log_line("control channel closed");
      return;
    }
    Reader reader{header, sizeof(header), 0};
    uint32_t magic = 0;
    uint32_t kind = 0;
    uint32_t flags = 0;
    uint32_t length = 0;
    if (!reader.u32(&magic) || !reader.u32(&kind) || !reader.u32(&flags) ||
        !reader.u32(&length) || magic != WVH_MAGIC) {
      std::cerr << "bad frame header" << std::endl;
      return;
    }
    std::vector<uint8_t> payload(length);
    if (length > 0 && !read_full(client_fd, payload.data(), length)) {
      log_line("control channel closed while reading a payload");
      return;
    }
    if (options.verbose) {
      log_line("command " + std::to_string(kind) + " bytes=" + std::to_string(length));
    }
    switch (kind) {
      case WVH_CMD_HELLO: {
        std::vector<uint8_t> reply;
        append_u32(&reply, WVH_VERSION);
        append_u32(&reply, WVH_SHM_HEADER_BYTES);
        append_u32(&reply, options.slot_count);
        append_u32(&reply, options.slot_size);
        events.push_event(WVH_EV_READY, reply);
        break;
      }
      case WVH_CMD_INIT: {
        Reader body{payload.data(), payload.size(), 0};
        wvh_init init;
        if (!body.u32(&init.allow_distinctive_identifier) ||
            !body.u32(&init.allow_persistent_state) ||
            !body.u32(&init.use_hw_secure_codecs)) {
          push_result(events, 0, WVH_ERROR, 0, 0);
          break;
        }
        cdm->Initialize(init.allow_distinctive_identifier != 0,
                        init.allow_persistent_state != 0,
                        init.use_hw_secure_codecs != 0);
        if (!host.wait_init()) {
          push_result(events, 0, WVH_TIMEOUT, 0, 0);
          break;
        }
        push_result(events, 0, host.init_success() ? WVH_OK : WVH_ERROR, 0, 0);
        break;
      }
      case WVH_CMD_SET_SERVER_CERTIFICATE: {
        Reader body{payload.data(), payload.size(), 0};
        uint32_t request_id = 0;
        std::vector<uint8_t> certificate;
        if (!body.u32(&request_id) || !body.blob(&certificate) || certificate.empty()) {
          push_result(events, request_id, WVH_ERROR, 0, 0);
          break;
        }
        cdm->SetServerCertificate(request_id, certificate.data(),
                                  static_cast<uint32_t>(certificate.size()));
        push_result(events, request_id,
                    host.wait_promise(request_id) ? WVH_OK : WVH_ERROR, 0, 0);
        break;
      }
      case WVH_CMD_CREATE_SESSION: {
        Reader body{payload.data(), payload.size(), 0};
        wvh_create_session request;
        if (!body.u32(&request.request_id) || !body.u32(&request.session_type) ||
            !body.u32(&request.init_data_type)) {
          push_result(events, 0, WVH_ERROR, 0, 0);
          break;
        }
        std::vector<uint8_t> init_data;
        if (!body.blob(&init_data)) {
          push_result(events, request.request_id, WVH_ERROR, 0, 0);
          break;
        }
        if (options.verbose) {
          std::string hex;
          for (size_t index = 0; index < init_data.size() && index < 12; ++index) {
            static const char* digits = "0123456789abcdef";
            hex.push_back(digits[init_data[index] >> 4]);
            hex.push_back(digits[init_data[index] & 15]);
          }
          log_line("create session type=" + std::to_string(request.session_type) +
                   " init_data_type=" + std::to_string(request.init_data_type) +
                   " bytes=" + std::to_string(init_data.size()) + " head=" + hex);
        }
        cdm->CreateSessionAndGenerateRequest(
            request.request_id, static_cast<cdm::SessionType>(request.session_type),
            static_cast<cdm::InitDataType>(request.init_data_type),
            init_data.empty() ? nullptr : init_data.data(),
            static_cast<uint32_t>(init_data.size()));
        std::string session_id;
        if (!host.wait_session(request.request_id, &session_id) || session_id.empty()) {
          push_result(events, request.request_id, WVH_ERROR, 0, 0);
          break;
        }
        push_result(events, request.request_id, WVH_OK, 0, 0);
        break;
      }
      case WVH_CMD_UPDATE_SESSION: {
        Reader body{payload.data(), payload.size(), 0};
        wvh_promise_request request;
        std::string session_id;
        std::vector<uint8_t> response;
        if (!body.u32(&request.request_id) || !body.text(&session_id) ||
            !body.blob(&response)) {
          push_result(events, 0, WVH_ERROR, 0, 0);
          break;
        }
        cdm->UpdateSession(request.request_id, session_id.data(),
                           static_cast<uint32_t>(session_id.size()),
                           response.empty() ? nullptr : response.data(),
                           static_cast<uint32_t>(response.size()));
        push_result(events, request.request_id,
                    host.wait_promise(request.request_id) ? WVH_OK : WVH_ERROR, 0, 0);
        break;
      }
      case WVH_CMD_CLOSE_SESSION:
      case WVH_CMD_REMOVE_SESSION: {
        Reader body{payload.data(), payload.size(), 0};
        wvh_promise_request request;
        std::string session_id;
        if (!body.u32(&request.request_id) || !body.text(&session_id)) {
          push_result(events, 0, WVH_ERROR, 0, 0);
          break;
        }
        if (kind == WVH_CMD_CLOSE_SESSION) {
          cdm->CloseSession(request.request_id, session_id.data(),
                            static_cast<uint32_t>(session_id.size()));
        } else {
          cdm->RemoveSession(request.request_id, session_id.data(),
                             static_cast<uint32_t>(session_id.size()));
        }
        push_result(events, request.request_id,
                    host.wait_promise(request.request_id) ? WVH_OK : WVH_ERROR, 0, 0);
        break;
      }
      case WVH_CMD_DECRYPT:
        handle_decrypt(options, host, events, region, slots, payload);
        break;
      case WVH_CMD_DECODE:
        handle_decode(options, host, events, region, slots, payload);
        break;
      case WVH_CMD_CONFIGURE_VIDEO:
        handle_configure_video(options, host, events, payload);
        break;
      case WVH_CMD_RELEASE: {
        Reader body{payload.data(), payload.size(), 0};
        uint32_t slot = 0;
        if (body.u32(&slot)) {
          slots.release(slot);
        }
        break;
      }
      case WVH_CMD_SHUTDOWN:
        push_result(events, 0, WVH_OK, 0, 0);
        running = false;
        break;
      default:
        push_result(events, 0, WVH_UNSUPPORTED, 0, 0);
        break;
    }
  }
}

}  // namespace

int main(int argc, char** argv) {
  Options options;
  if (!parse_arguments(argc, argv, &options)) {
    std::cerr << "usage: widevine-host --cdm PATH (--socket PATH | --fd N) "
                 "(--shm PATH | --shm-fd N) [--storage DIR] [--persistent] "
                 "[--slot-count N] [--slot-size BYTES] [--verbose]"
              << std::endl;
    return 1;
  }

  std::error_code error;
  if (options.persistent) {
    std::filesystem::create_directories(options.storage_dir, error);
  }

  SharedRegion region;
  const bool region_ok = !options.shm_path.empty()
                             ? region.open_path(options.shm_path, options.slot_count,
                                                options.slot_size)
                             : region.map_fd(options.shm_fd, options.slot_count,
                                             options.slot_size);
  if (!region_ok || !region.valid()) {
    std::cerr << "shared memory setup failed" << std::endl;
    return 2;
  }
  log_line("shared region ready: slots=" + std::to_string(options.slot_count) +
           " slot_size=" + std::to_string(options.slot_size));

  SlotPool slots(options.slot_count);
  EventSink events;

  void* library = dlopen(options.cdm_path.c_str(), RTLD_NOW | RTLD_LOCAL);
  if (library == nullptr) {
    std::cerr << "dlopen failed: " << dlerror() << std::endl;
    return 3;
  }
  auto initialize_module =
      reinterpret_cast<void (*)()>(dlsym(library, "InitializeCdmModule_4"));
  auto create_instance =
      reinterpret_cast<void* (*)(int, const char*, uint32_t, GetCdmHostFunc, void*)>(
          dlsym(library, "CreateCdmInstance"));
  auto get_version =
      reinterpret_cast<const char* (*)()>(dlsym(library, "GetCdmVersion"));
  if (create_instance == nullptr) {
    std::cerr << "CreateCdmInstance missing" << std::endl;
    return 3;
  }
  if (initialize_module != nullptr) {
    initialize_module();
  }
  if (get_version != nullptr) {
    log_line(std::string("cdm version ") + get_version());
  }

  auto live = std::make_shared<Liveness>();
  CdmHost host(options, region, slots, events);
  host.set_liveness(live);

  static const char kKeySystem[] = "com.widevine.alpha";
  void* raw = create_instance(ContentDecryptionModule::kVersion, kKeySystem,
                              static_cast<uint32_t>(strlen(kKeySystem)), &get_cdm_host, &host);
  if (raw == nullptr) {
    std::cerr << "CreateCdmInstance failed" << std::endl;
    return 4;
  }
  auto* cdm = static_cast<ContentDecryptionModule*>(raw);
  host.set_cdm(cdm);
  live->cdm = cdm;

  const int client_fd = open_control_socket(options);
  if (client_fd < 0) {
    return 5;
  }
  events.start(client_fd, options.verbose);
  log_line("client connected");

  serve(client_fd, options, host, events, region, slots, cdm);

  events.stop();
  {
    std::lock_guard<std::mutex> lock(live->mutex);
    live->destroyed = true;
    cdm->Destroy();
    live->cdm = nullptr;
  }
  close(client_fd);
  log_line("host stopped");
  return 0;
}
