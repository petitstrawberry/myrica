# widevine-host

Widevine CDM service process for Scarlet. It is the W1 component of the
Widevine track in `docs/encrypted-media-roadmap.md`: the CDM lives in a
process running inside a Linux ABI view, the Scarlet-native browser talks to
it over a control socket, and sample data crosses through a shared memory
region that the native side owns.

The host never contacts a licence server. It reports licence or
individualization requests to the client and applies whatever response the
client sends back, mirroring Chromium's out-of-process CDM split.

## Files

- `protocol.h`: framing, command and event numbers, payload layouts, and the
  shared region header.
- `widevine-host.cpp`: the CDM host. Implements `cdm::Host_10`, serializes CDM
  calls on the request loop, and writes slot-backed messages back to the
  client. `api/` headers come from `../widevine-probe/api`.
- `test-client.cpp`: a small driver used to exercise the protocol without
  Scarlet.
- `build.sh`: builds both for aarch64 Linux with the Scarlet toolchain.
- `test/run-in-container.sh`: runs the pair inside an aarch64 Debian
  container with a user-supplied CDM, which is how the current slice was
  verified.

## Protocol

Framing is a 16-byte header (`magic`, `kind`, `flags`, `length`) followed by
the payload. Integers are little-endian.

Requests: `HELLO`, `INIT`, `CREATE_SESSION`, `UPDATE_SESSION`,
`CLOSE_SESSION`, `REMOVE_SESSION`, `DECRYPT`, `RELEASE`, `SHUTDOWN`.

Events: `READY`, `RESULT`, `SESSION`, `MESSAGE`, `KEYS`, `EXPIRATION`,
`SESSION_CLOSED`. Events are asynchronous: a session id or licence request can
arrive before the result of the request that produced it, so clients must
dispatch on the frame kind rather than on a fixed reply order.

The shared region starts with a 4096-byte `wvh_shm_header` and continues with
`slot_count` slots of `slot_size` bytes (default 4 x 1 MiB). Message payloads
and decrypted samples are addressed as `[slot, offset, size]`; the client
returns a slot with `RELEASE`.

On Scarlet the native parent creates the `SharedMemory` object and a socket
pair, maps them into the child through `Environment::spawn` handle mappings
(`target` becomes the child's fd number), and starts the host with `--shm-fd`
and `--fd`. For local testing the host also accepts `--shm PATH` and
`--socket PATH`.

## Build

```sh
./build.sh /tmp/widevine-host
```

## Verify in a container

```sh
mkdir -p /tmp/nspr && cp /path/to/libnspr4.so /tmp/nspr/
NSPR_LIB_DIR=/tmp/nspr ./test/run-in-container.sh /tmp/widevine-host ~/.local/share/widevine
```

Expected tail:

```
READY version=1 header=4096 slots=4 slot_size=1048576
INIT status=0
EV_SESSION request=1 session=...
EV_MESSAGE type=0 slot=0 size=1677
CREATE_SESSION status=0
MESSAGE_BYTES 8 1 18 239 size=1677
CLIENT_OK
```

That is the CDM's licence request arriving through shared memory.

## Verify the licence round trip

The test client can POST the licence request to a relay that terminates TLS,
apply the response with UPDATE_SESSION, and report the key statuses.

    python3 test/license-relay.py 8788

Then run the container with the PSSH from the Shaka Sintel Widevine asset
(https://storage.googleapis.com/shaka-demo-assets/sintel-widevine/dash.mpd):

    /wvh/test-client --socket ... --shm ... --pssh-hex <167-byte pssh>       --license-relay http://host.docker.internal:8788/

Expected: LICENSE_RESPONSE_BYTES, five KEY lines with status=0 (usable),
UPDATE_SESSION status=0 and KEYS_REPORTED. Verified 2026-10-18: a 167-byte
Sintel PSSH produced a 1797-byte licence request, the Shaka test proxy
returned 1229 bytes, and the CDM reported five usable keys.

In the browser integration the page's own fetch() performs the licence POST,
so the browser needs no licence networking of its own; the relay exists only
so the container client can stay TLS-free.

## Status

- Working: CDM load and initialize, session creation, licence request delivery
  through shared memory, slot release, shutdown. Verified 2026-10-17 in an
  aarch64 Debian container with the ChromeOS CDM from
  `~/.local/share/widevine`.
- Not yet wired: the Scarlet-native client that spawns the host with handle
  mappings, licence provisioning over the network, and `DECRYPT` against real
  keys (the request path exists; a keyless call returns `WVH_NO_KEY`).
