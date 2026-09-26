# Myrica

Myrica is a cross-platform web browser built for Scarlet OS. ScarletUI owns the
browser chrome and platform integration; a deliberately small backend boundary
allows the embedded web engine to evolve independently.

The name comes from *Myrica rubra*, an evergreen tree with scarlet fruit.

## Current state

- ScarletUI browser chrome with an address field and navigation controls
- Native desktop frontend through ScarletUI's Winit backend
- Scarlet OS frontend through ScarletUI's SWS backend
- Blitz HTML/CSS rendering backend with HTTP(S) loading
- HTTP charset and Unicode BOM decoding for pages and external scripts
- Boa-powered JavaScript (enabled by default) for inline and external scripts, ES modules, DOM events, timers, fetch, and in-memory web storage
- Dedicated engine worker for HTML, JavaScript, layout, and SGFX scene preparation
- Page `input` and `textarea` editing, including multiline text and IME composition
- Backend-neutral navigation, input, lifecycle, and retained scene boundary

Blitz is the bring-up backend. Servo is the intended full browser engine once
its Scarlet platform dependencies have been identified and ported.

## Run on desktop

```bash
cargo run --release -- https://example.com/
```

The URL is optional and defaults to `https://example.com/`.
JavaScript is enabled by default. To run without it:

```bash
cargo run --release --no-default-features --features backend-blitz -- https://example.com/
```

This runtime supports a subset of browser JavaScript APIs; compatibility with
general websites is still limited.
Script-enabled documents suppress `noscript` fallback markup, and
`performance.now()`/`performance.timeOrigin` share the timer clock.

## Build for Scarlet OS

The included Nix flake follows the Scarlet application development environment
used by Boxcraft and supports x86_64/aarch64 Linux and Darwin hosts:

```bash
nix develop
cargo build --release --target aarch64-unknown-scarlet
cargo build --release --target riscv64gc-unknown-scarlet
```

The Scarlet builds also include JavaScript by default.

The Scarlet Rust toolchain input may temporarily lag compiler changes required
by Myrica's dependencies. Desktop development remains available with an
ordinary recent Rust toolchain while that input catches up.

## Architecture

`BrowserBackend` is intentionally a WebView boundary rather than a common DOM
API. It owns navigation and history, receives viewport input, advances engine
work, and prepares retained SGFX meshes and textures on one dedicated engine
thread. The UI sends commands and takes the latest completed frame without
waiting for HTML parsing, JavaScript, layout, or glyph rasterization. Pixel and
mesh buffers are shared without copying on the UI thread; ScarletUI owns GPU
presentation. Intermediate pointer moves and resize requests are coalesced,
while button and keyboard event order is preserved. Obsolete navigation and
viewport results cannot replace the current frame.

A long-running script can still delay work within its page; browser chrome,
window events, and closing the application remain on the UI thread.

To check responsiveness and input locally, serve `tests/fixtures` with an HTTP
server, open `worker-input.html`, and press **Run JavaScript for 8 seconds**.
The address field and window controls should remain responsive throughout the
busy loop. The same page has single-line and multiline fields for editing and
IME checks.

Backends are selected at compile time. This avoids linking multiple browser
engines into one process and keeps dependency conflicts isolated.

## License

Myrica is licensed under the [MIT License](LICENSE).
