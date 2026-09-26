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
- Optional Boa-powered JavaScript for inline and external scripts, ES modules, DOM events, timers, fetch, and in-memory web storage
- Backend-neutral navigation, input, lifecycle, and framebuffer boundary

Blitz is the bring-up backend. Servo is the intended full browser engine once
its Scarlet platform dependencies have been identified and ported.

## Run on desktop

```bash
cargo run --release -- https://example.com/
```

The URL is optional and defaults to `https://example.com/`.
Enable the experimental script runtime with `--features javascript`:

```bash
cargo run --release --features javascript -- https://example.com/
```

This runtime supports a subset of browser JavaScript APIs; compatibility with
general websites is still limited.

## Build for Scarlet OS

The included Nix flake follows the Scarlet application development environment
used by Boxcraft and supports x86_64/aarch64 Linux and Darwin hosts:

```bash
nix develop
cargo build --release --target aarch64-unknown-scarlet
cargo build --release --target riscv64gc-unknown-scarlet
```

Add `--features javascript` to either Scarlet build command to include the
script runtime.

The Scarlet Rust toolchain input may temporarily lag compiler changes required
by Myrica's dependencies. Desktop development remains available with an
ordinary recent Rust toolchain while that input catches up.

## Architecture

`BrowserBackend` is intentionally a WebView boundary rather than a common DOM
API. It owns navigation and history, receives viewport input, advances engine
work, and paints into a BGRA framebuffer supplied by ScarletUI. Engine-specific
DOM, scripting, networking, and graphics types do not leak into the app shell.

Backends are selected at compile time. This avoids linking multiple browser
engines into one process and keeps dependency conflicts isolated.

## License

Myrica is licensed under the [MIT License](LICENSE).
