#include "../task-runner.h"

#include <cassert>
#include <iostream>
#include <thread>
#include <vector>

int main() {
  CdmTaskRunner tasks;
  assert(tasks.valid());
  const auto owner = std::this_thread::get_id();
  std::vector<int> fired;
  tasks.post(20, [&] {
    assert(std::this_thread::get_id() == owner);
    fired.push_back(2);
    tasks.post(0, [&] { fired.push_back(3); });
  });
  tasks.post(-1, [&] { fired.push_back(1); });
  const auto deadline = CdmTaskRunner::Clock::now() + std::chrono::seconds(2);
  while (fired.size() < 3) {
    assert(tasks.wait(-1, deadline) == CdmTaskRunner::Wait::Ready);
  }
  assert((fired == std::vector<int>{1, 2, 3}));

  // A timer scheduled by another producer must wake the idle owner and still
  // execute on that owner, including when it is waiting for a promise.
  bool done = false;
  std::thread producer([&] {
    std::this_thread::sleep_for(std::chrono::milliseconds(10));
    tasks.post(0, [&] {
      assert(std::this_thread::get_id() == owner);
      done = true;
    });
  });
  while (!done) assert(tasks.wait(-1, deadline) == CdmTaskRunner::Wait::Ready);
  producer.join();

  // Timers also run while the control stream is idle or fragmented. Receiving
  // one byte must not wait for a complete protocol header.
  int socket[2];
  assert(::socketpair(AF_UNIX, SOCK_STREAM, 0, socket) == 0);
  tasks.post(10, [&] { assert(::write(socket[1], "a", 1) == 1); });
  assert(tasks.wait(socket[0], deadline) == CdmTaskRunner::Wait::Ready);
  char byte;
  assert(::read(socket[0], &byte, 1) == 1 && byte == 'a');
  tasks.post(10, [&] { assert(::write(socket[1], "b", 1) == 1); });
  assert(tasks.wait(socket[0], deadline) == CdmTaskRunner::Wait::Ready);
  assert(::read(socket[0], &byte, 1) == 1 && byte == 'b');
  ::close(socket[1]);
  assert(tasks.wait(socket[0], deadline) == CdmTaskRunner::Wait::Ready);
  assert(::read(socket[0], &byte, 1) == 0);
  ::close(socket[0]);

  // Drain stale state notifications before testing the bounded idle wait.
  auto short_deadline = CdmTaskRunner::Clock::now() + std::chrono::milliseconds(10);
  while (tasks.wait(-1, short_deadline) == CdmTaskRunner::Wait::Ready) {}
  assert(CdmTaskRunner::Clock::now() >= short_deadline);
  bool cancelled = true;
  {
    CdmTaskRunner temporary;
    temporary.post(0, [&] { cancelled = false; });
  }
  assert(cancelled);
  std::cout << "CDM task runner: owner thread, nested timer, cross-thread wake, partial stream, EOF, timeout and cancellation passed\n";
}
