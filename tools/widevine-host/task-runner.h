#pragma once

#include <poll.h>
#include <sys/socket.h>
#include <fcntl.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <climits>
#include <functional>
#include <map>
#include <mutex>

// All CDM entry points, including timer and host response callbacks, execute
// on the request loop. Producers may enqueue work from another thread.
class CdmTaskRunner {
 public:
  using Clock = std::chrono::steady_clock;
  using Deadline = Clock::time_point;
  enum class Wait { Ready, Timeout, Error };

  CdmTaskRunner() {
    if (::socketpair(AF_UNIX, SOCK_STREAM, 0, wake_) != 0) return;
    for (int fd : wake_) {
      if (::fcntl(fd, F_SETFD, FD_CLOEXEC) < 0 ||
          ::fcntl(fd, F_SETFL, O_NONBLOCK) < 0) {
        ::close(wake_[0]);
        ::close(wake_[1]);
        wake_[0] = wake_[1] = -1;
        break;
      }
    }
  }
  ~CdmTaskRunner() {
    for (int fd : wake_) if (fd >= 0) ::close(fd);
  }
  CdmTaskRunner(const CdmTaskRunner&) = delete;
  CdmTaskRunner& operator=(const CdmTaskRunner&) = delete;
  bool valid() const { return wake_[0] >= 0; }

  void wake() {
    const char byte = 0;
    while (::write(wake_[1], &byte, 1) < 0 && errno == EINTR) {}
    // EAGAIN means the socket already contains a wake notification.
  }
  void post(int64_t delay_ms, std::function<void()> callback) {
    {
      std::lock_guard<std::mutex> lock(mutex_);
      auto now = Clock::now();
      auto delay = std::chrono::milliseconds(std::max<int64_t>(0, delay_ms));
      auto maximum = std::chrono::duration_cast<std::chrono::milliseconds>(Deadline::max() - now);
      tasks_.emplace(delay >= maximum ? Deadline::max() : now + delay, std::move(callback));
    }
    wake();
  }

  // A negative fd waits only for callbacks/state notifications. Timer work is
  // serviced while idle, while reading partial frames, and while awaiting CDM
  // initialization or promises. No worker thread calls into the CDM.
  Wait wait(int fd, Deadline deadline = Deadline::max()) {
    for (;;) {
      if (run_ready() && fd < 0) return Wait::Ready;
      auto now = Clock::now();
      if (now >= deadline) return Wait::Timeout;
      auto next = deadline;
      {
        std::lock_guard<std::mutex> lock(mutex_);
        if (!tasks_.empty()) next = std::min(next, tasks_.begin()->first);
      }
      int timeout = -1;
      if (next != Deadline::max()) {
        auto remaining = std::chrono::ceil<std::chrono::milliseconds>(next - now).count();
        timeout = static_cast<int>(std::clamp<int64_t>(remaining, 0, INT_MAX));
      }
      pollfd fds[] = {{fd, POLLIN, 0}, {wake_[0], POLLIN, 0}};
      int result = ::poll(fds, 2, timeout);
      if (result < 0) {
        if (errno == EINTR) continue;
        return Wait::Error;
      }
      if (fds[1].revents & (POLLERR | POLLHUP | POLLNVAL)) return Wait::Error;
      if (fds[1].revents & POLLIN) {
        char bytes[256];
        while (::read(wake_[0], bytes, sizeof(bytes)) > 0) {}
        run_ready();
      }
      if (fds[0].revents & (POLLERR | POLLNVAL)) return Wait::Error;
      if (fds[0].revents & (POLLIN | POLLHUP)) return Wait::Ready;
      // Return control to the predicate waiter for a state-only wake.
      if (fd < 0 && result > 0) return Wait::Ready;
    }
  }

 private:
  bool run_ready() {
    bool ran = false;
    for (;;) {
      std::function<void()> callback;
      {
        std::lock_guard<std::mutex> lock(mutex_);
        if (tasks_.empty() || tasks_.begin()->first > Clock::now()) return ran;
        auto task = tasks_.extract(tasks_.begin());
        callback = std::move(task.mapped());
      }
      callback();
      ran = true;
    }
  }
  int wake_[2] = {-1, -1};
  std::mutex mutex_;
  std::multimap<Deadline, std::function<void()>> tasks_;
};
