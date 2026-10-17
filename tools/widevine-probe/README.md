# widevine-probe

Minimal Widevine CDM host used to check that a proprietary `libwidevinecdm.so`
can load, initialize, exchange a licence request, and stay alive inside a given
Linux userspace. It is the W0 tool of the Widevine track described in
`docs/encrypted-media-roadmap.md` (myrica repository).

The probe implements Chromium's CDM ABI directly: `InitializeCdmModule_4`,
`CreateCdmInstance`, `cdm::ContentDecryptionModule_10` and `cdm::Host_10`.
The headers under `api/` are Chromium's CDM interface (BSD-3, from
chromium.googlesource.com/chromium/cdm).

## The CDM binary

The probe does not ship a Widevine CDM and never should. The CDM is
proprietary; Google's licence forbids redistribution without a separate
agreement. Each user supplies their own copy:

- Debian (aarch64) installs one through the `widevine-installer` package
  (trixie-backports): `sudo widevine-installer` places it under
  `/var/lib/widevine/WidevineCdm/<version>/_platform_specific/linux_arm64/`.
- Any ARM64 ChromeOS/LaCrOS image contains
  `/opt/google/chrome/WidevineCdm/_platform_specific/cros_arm64/libwidevinecdm.so`.

### Fix the binary before use

ChromeOS CDM builds do not load on current glibc:

- `DT_RELR without GLIBC_ABI_DT_RELR dependency` (glibc 2.36 and later).
- Missing `__aarch64_ldadd4_acq_rel` and `__aarch64_swp4_acq_rel`
  (libgcc no longer exports the out-of-line atomics the CDM calls).

Asahi Linux's `widevine_fixup.py` (MIT, David Buchanan) injects both. On
4 KiB-page systems that path keeps RELRO and W^X intact; on 16 KiB-page
systems the same tool also rewrites segment alignment, which disables RELRO,
so decide that yourself.

```sh
python3 prepare-cdm.py libwidevinecdm.so libwidevinecdm-fixed.so
```

The fixed library also needs `libnspr4` at run time (`apt install libnspr4`).

## Build

```sh
./build.sh
```

The script uses `aarch64-unknown-linux-gnu-g++` from the Scarlet development
shell and sets `/lib/ld-linux-aarch64.so.1` as the interpreter so the binary
runs in ordinary Debian userlands. To build inside a container instead:

```sh
docker run --rm -v "$PWD:/src" debian:trixie-slim sh -c \
  "apt-get update -qq && apt-get install -y -qq g++ && g++ -O2 -std=c++17 -o /src/widevine-probe /src/main.cpp -ldl"
```

## Run: container sanity check

```sh
docker run --rm -v /path/to/kit:/wv debian:trixie-slim sh -c \
  "apt-get update -qq && apt-get install -y -qq libnspr4; /wv/widevine-probe --cdm /wv/libwidevinecdm-fixed.so --kid-hex 00112233445566778899aabbccddeeff --verbose"
```

Expected tail: `PROBE_STAGE initialize ok`, a `MESSAGE_BASE64 license-request`
line, and `PROBE_RESULT ok`.

## Run: Scarlet Linux ABI view

Copy the probe, the fixed CDM and any response files to `/shared/widevine`,
which both the native view and the `linux-aarch64` view can see, then run:

```sh
abi-run linux-aarch64 /shared/widevine/widevine-probe --cdm /shared/widevine/libwidevinecdm-fixed.so --kid-hex 00112233445566778899aabbccddeeff --verbose
```

A native launcher can do the same through `scarlet_os::environment` and pass
socket or pipe handles explicitly. The probe reports which stage ran, so a
partial result still identifies the failing syscall family.

## Options

| Option | Meaning |
| --- | --- |
| `--cdm PATH` | CDM library to load (default: `WIDEVINE_CDM_PATH`, then the `/var/lib/widevine` layout) |
| `--kid-hex HEX` | synthesize a Widevine pssh for this 16-byte key id |
| `--pssh-hex HEX` | cenc init data for `CreateSessionAndGenerateRequest` |
| `--update-hex HEX` | `UpdateSession` payload (provisioning or licence response) |
| `--update-file FILE` | same payload from a file |
| `--persistent` | request `allow_persistent_state=true` |
| `--storage DIR` | FileIO root for persistent state (default `widevine-storage`) |
| `--dump-dir DIR` | write received messages and the last message to `DIR` |
| `--timeout SECONDS` | per-stage wait budget (default 15) |
| `--verbose` | log every host callback |

## Exit codes

`0` ok, `1` usage, `2` load or entry points, `3` `CreateCdmInstance`,
`4` `Initialize`, `5` session or message, `6` `UpdateSession`.

## Known limits

- The probe never talks to a licence server. W1 of the roadmap adds
  provisioning and licence exchange; `--update-file` is the hook for a
  response fetched by hand.
- `SetTimer` runs one detached thread per timer; Widevine only uses timers
  for licence expiry, so they stay silent during bring-up.
- `SendPlatformChallenge` is logged and not answered, which matches desktop
  L3 behaviour.
