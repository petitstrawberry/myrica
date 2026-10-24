#!/bin/sh
# Build widevine-probe for aarch64 Linux (glibc) with the Scarlet toolchain.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cxx=${CROSS_CXX:-aarch64-unknown-linux-gnu-g++}
command -v "$cxx" >/dev/null 2>&1 || {
  echo "$cxx not found; enter the Scarlet development shell first" >&2
  exit 1
}
exec "$cxx" -O2 -std=c++17 -Wall -Wextra -static-libstdc++ -static-libgcc \
  -Wl,--dynamic-linker=/lib/ld-linux-aarch64.so.1 \
  -o "$here/widevine-probe" "$here/main.cpp" -ldl
