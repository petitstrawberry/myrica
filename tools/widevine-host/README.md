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

## Verify decryption with real content

extract-sample.py pulls one encrypted sample, its IV and its subsample map out
of a fragmented CENC segment pair:

    python3 test/extract-sample.py --init init.mp4 --media 0001.m4s --out-dir out

The test client decrypts that sample once the licence has been applied:

    /wvh/test-client ... --license-relay http://host.docker.internal:8790/       --decrypt-sample /wvh/ax/sample.bin --decrypt-iv <iv>       --decrypt-key-id <kid> --decrypt-subsamples 5:754,5:6263

Verified 2026-10-18 with the Axinom v7-MultiDRM-SingleKey test vector
(https://media.axprod.net/TestVectors/v7-MultiDRM-SingleKey/Manifest.mpd and
https://drm-widevine-licensing.axtest.net/AcquireLicense with the published
X-AxDRM-Message test entitlement):

- the licence response carried one usable key (status=0),
- the audio sample (341 bytes, no subsample map) decrypted with kSuccess,
- the video sample came back with a byte-exact first NAL length (755) when the
  request carried a single cipher region,
- a wrong key id returns kNoKey, so the success path is key driven.

The early probe interpreted multi-entry failures as a CDM limitation. W3
corrected the native client's input assembly: CENC cipher bytes from every
subsample form one continuous stream, with clear bytes excluded. The client
gathers that stream into one Decrypt request and scatters the result into the
original sample. Reusing the IV separately for each region was incorrect.
The previous zero-output retry has been removed.

## Status

The Scarlet-native client in `crates/cdm-host` spawns this process using handle
mappings. WebCore delivers container init data, the page performs the licence
exchange, and the media worker decrypts before the native `/dev/video0` decoder.
With the same CDM 4.10.2662.3, Scarlet Release validation passed two encrypted
four-second Sintel fragments (94 decoded frames each) and the sixteen-second
clear-lead transition (382 frames). Details and current format/session limits
are in [the roadmap](../../docs/encrypted-media-roadmap.md#w3-メディア接続と実測-2026-10-18).

The test client's raw `--decrypt-sample` / experimental decode modes are W1
probes of CDM API behaviour. The browser playback regression is
`tests/eme-widevine-playback.html`, served by `tests/webkit_server.py` on the
host and exercised by the Scarlet `webkit-smoke --eme-only` example.
