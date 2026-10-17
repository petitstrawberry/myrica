#!/bin/sh
# Run widevine-host and the test client inside an aarch64 Debian container
# against the Widevine CDM from the local kit.
#
# usage: NSPR_LIB_DIR=/path/with/libnspr4.so run-in-container.sh BIN_DIR KIT_DIR
#
# NSPR_LIB_DIR must live outside /nix: Docker Desktop file sharing can stall on
# the Nix store path. Copy libnspr4.so to a normal directory first.
set -eu
bin_dir=${1:?usage: run-in-container.sh BIN_DIR KIT_DIR}
kit_dir=${2:?usage: run-in-container.sh BIN_DIR KIT_DIR}
nspr_dir=${NSPR_LIB_DIR:?set NSPR_LIB_DIR to a directory holding libnspr4.so}
exec docker run --rm -v "$bin_dir:/wvh" -v "$kit_dir:/wv:ro" -v "$nspr_dir:/nspr:ro" debian:trixie-slim sh -c '
  export LD_LIBRARY_PATH=/nspr
  mkdir -p /tmp/run; cd /tmp/run
  /wvh/widevine-host --cdm /wv/libwidevinecdm-fixed.so --socket /tmp/run/ctl.sock --shm /tmp/run/region.bin --verbose > host.log 2>&1 &
  sleep 2
  /wvh/test-client --socket /tmp/run/ctl.sock --shm /tmp/run/region.bin
  echo ---
  tail -8 /tmp/run/host.log
'
