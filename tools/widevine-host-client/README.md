# widevine-host-client

Scarlet-native side of the Widevine split. The crate ships two targets:

- `cdm_host` (library): owns the widevine-host process for myrica's WebKit EME
  integration. The browser never loads the CDM itself.
- `widevine-host-client` (binary): command line driver used for bring-up runs
  against a real CDM (licence exchange, sample extraction, decode checks).

## Library

`CdmHost::spawn` creates the SharedMemory region and the socket pair, spawns
widevine-host inside the linux-aarch64 view with stdio and both objects mapped
to descriptors 0 to 4, and completes the HELLO/READY handshake. The rest of the
surface is `initialize`, `create_session` (init data in, session id and the
CDM's licence request out), `update_session` (licence response in, key statuses
out), `close_session`, `decrypt` and `shutdown`, plus `HostEvent` for
asynchronous notifications.

`decrypt` splits a sample into its subsample regions and calls the CDM once per
region with the sample IV, because the CDM refuses a subsample map with more
than one entry; a zero-filled success is retried once.

The library targets aarch64-unknown-scarlet and links only scarlet-os and
scarlet-sys:

    nix develop /path/to/Scarlet -c cargo build --release --target aarch64-unknown-scarlet --lib

## Command line driver

    widevine-host-client --host /shared/wv/widevine-host --cdm /shared/wv/c \
      [--host-ld-path /shared/wv/nspr] [--pssh-hex HEX] [--license-relay URL] \
      [--decrypt-sample FILE --decrypt-iv HEX --decrypt-key-id HEX --decrypt-subsamples a:b,...] \
      [--decode-media FILE --decode-samples FILE --decode-mode decrypt|decrypt-full|decode]

The driver was used for the W1 measurements on Scarlet: probe licence request
delivery through shared memory, apply a licence from a test license server via
a host-side TLS relay, and decrypt or decode real Axinom test-vector samples.

## Wiring into WebKit (planned)

The fork-side EME implementation calls this library through the ScarletWebView
C ABI. Planned entry points: create/destroy a host handle, initialize,
create_session, update_session, close_session, decrypt and take_event. The
fork implements CDMFactoryScarlet, CDMPrivateScarlet, CDMInstanceScarlet and
CDMInstanceSessionScarlet on top; init data comes from InitDataRegistry and the
licence POST stays in the page's fetch().
