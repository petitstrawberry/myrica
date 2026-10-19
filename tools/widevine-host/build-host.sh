#!/bin/sh
# Build the Linux-view service. The caller owns the output path.
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: "${1:?usage: build-host.sh OUTPUT_FILE}"
cxx=${CROSS_CXX:-aarch64-unknown-linux-gnu-g++}
mkdir -p "$(dirname -- "$1")"
exec "$cxx" -O2 -std=c++17 -Wall -Wextra -static-libstdc++ -static-libgcc \
  -Wl,--dynamic-linker=/lib/ld-linux-aarch64.so.1 \
  -I "$here/../widevine-probe" -o "$1" "$here/widevine-host.cpp" -ldl
