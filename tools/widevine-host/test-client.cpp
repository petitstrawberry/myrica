// Test client for widevine-host. It exercises the control protocol and the
// shared memory data path without Scarlet: the host runs as a child process in
// the same Linux userspace, so the inherited-descriptor path (--shm-fd) and
// the path-based path (--shm, --socket) both stay testable.
//
// usage: test-client --socket PATH --shm PATH [--kid-hex HEX] [--cdm PATH]

#include "protocol.h"

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/un.h>
#include <unistd.h>

#include <cstdint>
#include <cstring>
#include <iostream>
#include <string>
#include <vector>

namespace {

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
  if (text.size() % 2 != 0) {
    return false;
  }
  out->clear();
  for (size_t index = 0; index < text.size(); index += 2) {
    const int high = hex_digit(text[index]);
    const int low = hex_digit(text[index + 1]);
    if (high < 0 || low < 0) {
      return false;
    }
    out->push_back(static_cast<uint8_t>((high << 4) | low));
  }
  return true;
}

void append_u32(std::vector<uint8_t>* out, uint32_t value) {
  out->push_back(static_cast<uint8_t>(value));
  out->push_back(static_cast<uint8_t>(value >> 8));
  out->push_back(static_cast<uint8_t>(value >> 16));
  out->push_back(static_cast<uint8_t>(value >> 24));
}

void append_bytes(std::vector<uint8_t>* out, const void* data, size_t size) {
  const uint8_t* bytes = static_cast<const uint8_t*>(data);
  out->insert(out->end(), bytes, bytes + size);
}

void append_string(std::vector<uint8_t>* out, const std::string& value) {
  append_u32(out, static_cast<uint32_t>(value.size()));
  append_bytes(out, value.data(), value.size());
}

std::vector<uint8_t> frame(uint32_t kind, const std::vector<uint8_t>& payload) {
  std::vector<uint8_t> out;
  append_u32(&out, WVH_MAGIC);
  append_u32(&out, kind);
  append_u32(&out, 0);
  append_u32(&out, static_cast<uint32_t>(payload.size()));
  out.insert(out.end(), payload.begin(), payload.end());
  return out;
}

bool read_full(int fd, void* buffer, size_t size) {
  uint8_t* bytes = static_cast<uint8_t*>(buffer);
  size_t done = 0;
  while (done < size) {
    const ssize_t got = read(fd, bytes + done, size - done);
    if (got <= 0) {
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
    const ssize_t put = write(fd, bytes + done, size - done);
    if (put <= 0) {
      return false;
    }
    done += static_cast<size_t>(put);
  }
  return true;
}

struct Frame {
  uint32_t kind = 0;
  std::vector<uint8_t> payload;
};

bool read_frame(int fd, Frame* frame) {
  uint8_t header[16];
  if (!read_full(fd, header, sizeof(header))) {
    return false;
  }
  uint32_t magic = 0;
  uint32_t length = 0;
  memcpy(&magic, header, 4);
  memcpy(&length, header + 12, 4);
  if (magic != WVH_MAGIC) {
    return false;
  }
  memcpy(&frame->kind, header + 4, 4);
  frame->payload.assign(length, 0);
  return length == 0 || read_full(fd, frame->payload.data(), length);
}

bool send_frame(int fd, uint32_t kind, const std::vector<uint8_t>& payload) {
  const std::vector<uint8_t> bytes = frame(kind, payload);
  return write_full(fd, bytes.data(), bytes.size());
}

bool take_u32(const std::vector<uint8_t>& payload, size_t* offset, uint32_t* value) {
  if (*offset + 4 > payload.size()) {
    return false;
  }
  memcpy(value, payload.data() + *offset, 4);
  *offset += 4;
  return true;
}

bool take_string(const std::vector<uint8_t>& payload, size_t* offset, std::string* value) {
  uint32_t size = 0;
  if (!take_u32(payload, offset, &size) || *offset + size > payload.size()) {
    return false;
  }
  value->assign(reinterpret_cast<const char*>(payload.data() + *offset), size);
  *offset += size;
  return true;
}

void append_u32_be(std::vector<uint8_t>* out, uint32_t value) {
  out->push_back(static_cast<uint8_t>(value >> 24));
  out->push_back(static_cast<uint8_t>(value >> 16));
  out->push_back(static_cast<uint8_t>(value >> 8));
  out->push_back(static_cast<uint8_t>(value));
}

std::vector<uint8_t> make_widevine_pssh(const std::vector<uint8_t>& key_id) {
  static const uint8_t kSystemId[16] = {0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce,
                                        0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed};
  std::vector<uint8_t> payload;
  payload.push_back(0x0a);
  payload.push_back(static_cast<uint8_t>(key_id.size()));
  payload.insert(payload.end(), key_id.begin(), key_id.end());
  const uint32_t box_size =
      4 + 4 + 4 + 16 + 4 + static_cast<uint32_t>(payload.size());
  std::vector<uint8_t> box;
  append_u32_be(&box, box_size);
  box.push_back('p');
  box.push_back('s');
  box.push_back('s');
  box.push_back('h');
  append_u32_be(&box, 0);
  box.insert(box.end(), kSystemId, kSystemId + 16);
  append_u32_be(&box, static_cast<uint32_t>(payload.size()));
  box.insert(box.end(), payload.begin(), payload.end());
  return box;
}

}  // namespace

struct Collected {
  std::string session_id;
  bool has_message = false;
  uint32_t message_type = 0;
  uint32_t slot = 0;
  uint32_t offset = 0;
  uint32_t size = 0;
};

void handle_event(const Frame& frame, Collected* collected) {
  size_t offset = 0;
  if (frame.kind == WVH_EV_SESSION) {
    uint32_t request_id = 0;
    take_u32(frame.payload, &offset, &request_id);
    take_string(frame.payload, &offset, &collected->session_id);
    std::cout << "EV_SESSION request=" << request_id
              << " session=" << collected->session_id << std::endl;
  } else if (frame.kind == WVH_EV_MESSAGE) {
    take_string(frame.payload, &offset, &collected->session_id);
    take_u32(frame.payload, &offset, &collected->message_type);
    take_u32(frame.payload, &offset, &collected->slot);
    take_u32(frame.payload, &offset, &collected->offset);
    take_u32(frame.payload, &offset, &collected->size);
    collected->has_message = true;
    std::cout << "EV_MESSAGE type=" << collected->message_type
              << " slot=" << collected->slot << " size=" << collected->size << std::endl;
  } else if (frame.kind == WVH_EV_KEYS) {
    std::cout << "EV_KEYS" << std::endl;
  } else if (frame.kind == WVH_EV_EXPIRATION) {
    std::cout << "EV_EXPIRATION" << std::endl;
  } else if (frame.kind == WVH_EV_SESSION_CLOSED) {
    std::cout << "EV_SESSION_CLOSED" << std::endl;
  } else {
    std::cout << "EV_UNKNOWN kind=" << frame.kind << std::endl;
  }
}

bool wait_result(int fd, uint32_t request_id, int32_t* status, uint32_t* value0,
                 uint32_t* value1, Collected* collected) {
  for (;;) {
    Frame frame;
    if (!read_frame(fd, &frame)) {
      return false;
    }
    if (frame.kind != WVH_EV_RESULT) {
      handle_event(frame, collected);
      continue;
    }
    size_t offset = 0;
    uint32_t id = 0;
    uint32_t raw_status = 0;
    take_u32(frame.payload, &offset, &id);
    take_u32(frame.payload, &offset, &raw_status);
    take_u32(frame.payload, &offset, value0);
    take_u32(frame.payload, &offset, value1);
    if (id != request_id) {
      std::cout << "EV_RESULT for other request=" << id << std::endl;
      continue;
    }
    *status = static_cast<int32_t>(raw_status);
    return true;
  }
}

bool wait_message(int fd, Collected* collected) {
  while (!collected->has_message) {
    Frame frame;
    if (!read_frame(fd, &frame)) {
      return false;
    }
    if (frame.kind == WVH_EV_RESULT) {
      continue;
    }
    handle_event(frame, collected);
  }
  return true;
}

int main(int argc, char** argv) {
  std::string socket_path;
  std::string shm_path;
  std::string kid_hex = "00112233445566778899aabbccddeeff";
  for (int index = 1; index < argc; ++index) {
    const std::string argument = argv[index];
    if (index + 1 >= argc) {
      break;
    }
    if (argument == "--socket") {
      socket_path = argv[++index];
    } else if (argument == "--shm") {
      shm_path = argv[++index];
    } else if (argument == "--kid-hex") {
      kid_hex = argv[++index];
    }
  }
  if (socket_path.empty() || shm_path.empty()) {
    std::cerr << "usage: test-client --socket PATH --shm PATH [--kid-hex HEX]" << std::endl;
    return 1;
  }

  const int shm_fd = open(shm_path.c_str(), O_RDWR | O_CREAT, 0600);
  if (shm_fd < 0) {
    std::cerr << "open shm failed" << std::endl;
    return 1;
  }
  const size_t shm_size = WVH_SHM_HEADER_BYTES + 4u * 1024u * 1024u;
  if (ftruncate(shm_fd, static_cast<off_t>(shm_size)) != 0) {
    std::cerr << "ftruncate shm failed" << std::endl;
    return 1;
  }
  void* region = mmap(nullptr, shm_size, PROT_READ | PROT_WRITE, MAP_SHARED, shm_fd, 0);
  if (region == MAP_FAILED) {
    std::cerr << "mmap shm failed" << std::endl;
    return 1;
  }

  const int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  if (fd < 0) {
    std::cerr << "socket failed" << std::endl;
    return 1;
  }
  sockaddr_un address;
  memset(&address, 0, sizeof(address));
  address.sun_family = AF_UNIX;
  memcpy(address.sun_path, socket_path.c_str(), socket_path.size());
  for (int attempt = 0; attempt < 200; ++attempt) {
    if (connect(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0) {
      break;
    }
    usleep(50000);
  }

  timeval timeout;
  timeout.tv_sec = 30;
  timeout.tv_usec = 0;
  setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));

  std::vector<uint8_t> hello;
  append_u32(&hello, WVH_VERSION);
  if (!send_frame(fd, WVH_CMD_HELLO, hello)) {
    std::cerr << "HELLO send failed" << std::endl;
    return 1;
  }

  Frame reply;
  if (!read_frame(fd, &reply) || reply.kind != WVH_EV_READY) {
    std::cerr << "no READY event" << std::endl;
    return 1;
  }
  size_t offset = 0;
  uint32_t host_version = 0;
  uint32_t header_bytes = 0;
  uint32_t slot_count = 0;
  uint32_t slot_size = 0;
  take_u32(reply.payload, &offset, &host_version);
  take_u32(reply.payload, &offset, &header_bytes);
  take_u32(reply.payload, &offset, &slot_count);
  take_u32(reply.payload, &offset, &slot_size);
  std::cout << "READY version=" << host_version << " header=" << header_bytes
            << " slots=" << slot_count << " slot_size=" << slot_size << std::endl;

  Collected collected;
  int32_t status = 0;
  uint32_t value0 = 0;
  uint32_t value1 = 0;

  std::vector<uint8_t> init;
  append_u32(&init, 0);
  append_u32(&init, 0);
  append_u32(&init, 0);
  if (!send_frame(fd, WVH_CMD_INIT, init)) {
    std::cerr << "INIT send failed" << std::endl;
    return 1;
  }
  if (!wait_result(fd, 0, &status, &value0, &value1, &collected)) {
    std::cerr << "INIT reply missing" << std::endl;
    return 1;
  }
  std::cout << "INIT status=" << status << std::endl;
  if (status != WVH_OK) {
    return 1;
  }

  std::vector<uint8_t> kid;
  if (!decode_hex(kid_hex, &kid) || kid.size() != 16) {
    std::cerr << "invalid --kid-hex" << std::endl;
    return 1;
  }
  const std::vector<uint8_t> init_data = make_widevine_pssh(kid);
  std::vector<uint8_t> create;
  append_u32(&create, 1);
  append_u32(&create, WVH_SESSION_TEMPORARY);
  append_u32(&create, WVH_INIT_DATA_CENC);
  append_u32(&create, static_cast<uint32_t>(init_data.size()));
  append_bytes(&create, init_data.data(), init_data.size());
  if (!send_frame(fd, WVH_CMD_CREATE_SESSION, create)) {
    std::cerr << "CREATE_SESSION send failed" << std::endl;
    return 1;
  }
  if (!wait_result(fd, 1, &status, &value0, &value1, &collected)) {
    std::cerr << "CREATE_SESSION reply missing" << std::endl;
    return 1;
  }
  std::cout << "CREATE_SESSION status=" << status << std::endl;
  if (status != WVH_OK) {
    return 1;
  }
  if (!wait_message(fd, &collected)) {
    std::cerr << "message event missing" << std::endl;
    return 1;
  }

  const uint8_t* message = static_cast<const uint8_t*>(region) + WVH_SHM_HEADER_BYTES +
                           static_cast<size_t>(collected.slot) * slot_size +
                           collected.offset;
  std::cout << "MESSAGE_BYTES " << static_cast<int>(message[0]) << " "
            << static_cast<int>(message[1]) << " " << static_cast<int>(message[2])
            << " " << static_cast<int>(message[3]) << " size=" << collected.size
            << std::endl;

  std::vector<uint8_t> release;
  append_u32(&release, collected.slot);
  send_frame(fd, WVH_CMD_RELEASE, release);
  send_frame(fd, WVH_CMD_SHUTDOWN, std::vector<uint8_t>());
  std::cout << "CLIENT_OK" << std::endl;
  return 0;
}
