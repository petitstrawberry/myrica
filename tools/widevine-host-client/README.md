# widevine-host-client

Scarlet-native diagnostic CLI for the reusable client in
[`crates/cdm-host`](../../crates/cdm-host). The same library is used by the
WebKit EME bridge; this CLI does not contain a second IPC implementation.
The CDM runs in `widevine-host` in the Linux ABI view.

Build from Myrica's development shell:

```sh
cargo build --release --target aarch64-unknown-scarlet \
  --manifest-path tools/widevine-host-client/Cargo.toml
```

Run in Scarlet with a user-provided CDM:

```sh
widevine-host-client --host /shared/widevine/widevine-host \
  --cdm /shared/widevine/libwidevinecdm.so \
  --host-ld-path /shared/widevine --sessions 6
```

Options are `--host`, `--cdm`, `--host-ld-path`, `--kid-hex` (16-byte hex
KID), `--slots`, `--slot-size` and `--sessions` (default 1, maximum 100).
Each session must produce exactly one initial message, then close successfully.
`WVHC_OK sessions=6` verifies reuse beyond the default four shared slots.

The library provides initialize / create_session / update_session /
close_session / decrypt / shutdown. The Linux C++ test client in
[`tools/widevine-host`](../widevine-host) has the licence-relay and sample-decrypt
options. Browser-level licence validation uses `tests/eme-widevine.html` and
`webkit-smoke URL --eme-only`; see the
[roadmap](../../docs/encrypted-media-roadmap.md).
