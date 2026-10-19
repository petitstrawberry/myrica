#!/bin/sh
# Build widevine-host and its test client for aarch64 Linux (glibc).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cxx=${CROSS_CXX:-aarch64-unknown-linux-gnu-g++}
command -v "$cxx" >/dev/null 2>&1 || {
  echo "$cxx not found; enter the Scarlet development shell first" >&2
  exit 1
}
out=${1:-$here/../../target/widevine-host}
mkdir -p "$out"
flags="-O2 -std=c++17 -Wall -Wextra -static-libstdc++ -static-libgcc -Wl,--dynamic-linker=/lib/ld-linux-aarch64.so.1"
sh "$here/build-host.sh" "$out/widevine-host"
"$cxx" $flags -w -I "$here" -o "$out/test-client" "$here/test-client.cpp"
echo "built $out/widevine-host"
echo "built $out/test-client"
