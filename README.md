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
- Optional native WebKit backend for AArch64 Scarlet, using a persistent WebCore Page

Blitz remains the default engine. The `backend-webkit` feature embeds WebKit's
Scarlet port through its C ABI; the same ScarletUI chrome can select either engine.

## Run on desktop

```bash
cargo run --release -- https://example.com/
```

The URL is optional and defaults to `https://example.com/`.
Use Ctrl+R to reload and Alt+Left/Alt+Right to move through history, including
while the address field is focused. The address field accepts a new address with Enter.
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

## WebKit on Scarlet

The experimental backend uses the Scarlet branch of the
[WebKit fork](https://github.com/petitstrawberry/WebKit/tree/scarlet) and its
`ScarletWebView` C ABI with JavaScriptCore.

Scarlet's desktop bundle selects the Myrica Cargo package. Myrica owns the
WebKit integration; Scarlet's flake provides common compilers and the SDK
runtime, not application source pins or engine packages.

`myrica-webkit-sys` prepares the native installation from its Cargo build script
when `backend-webkit` is enabled. CMake FetchContent retrieves the pinned WebKit
fork and upstream font sources inside `OUT_DIR`. The WebKit fork's dependency
producer owns Cairo, ICU, Fontconfig, SQLite and their source pins. The SDK owns
libc, libc++ and libc++abi. Myrica supplies the shared-memory C ABI through its
Rust dependency, alongside its embedding adapter. No nested Cargo build or
Scarlet source checkout is required.

All engine build files, downloaded sources and installed archives remain in
Cargo's build directory. The standard Scarlet development shell provides the
common SDK runtime and raw compilers. `MYRICA_WEBKIT_LINK_MANIFEST` can explicitly
select a prebuilt installation; otherwise Cargo builds and uses its own link
manifest. The SGFX userspace Cargo configuration supplies ordinary native
loader link flags. Building this CPU Cairo profile does not enable GPU rendering.

The Scarlet desktop bundle selects WebKit on AArch64. The default Cargo features
retain Blitz for desktop development and other Scarlet architectures.

Run the installed binary on Scarlet:

```bash
myrica --backend webkit https://ichigo.dev/
```

To include both engines, build with the default features plus `backend-webkit`.
Select `--backend blitz` or `--backend webkit` at startup. Omitting `--backend`
keeps Blitz when it is available. WebKit uses JavaScriptCore's native C-loop
interpreter; Myrica's `javascript` feature controls the Blitz/Boa engine only.

WebCore performs navigation, redirects, resource policy, HTML/CSS layout,
JavaScript, editing, and history. Myrica provides verified HTTP(S) transport and
uploads WebCore's Cairo framebuffer into its SGFX canvas. The font configuration
defaults to `/etc/fonts/fonts.conf`; `MYRICA_WEBKIT_FONT_CONFIG` can select another
installed configuration. Install its referenced fonts in the guest.

The Scarlet WebKit profile uses WebKit storage namespaces for localStorage and
sessionStorage, a shared native CookieJarDB for HTTP and document.cookie, and
BlobRegistryImpl for object URLs. Local storage and persistent cookies use
$HOME/.local/share/myrica. Subframes retain WebCore sandbox and origin policies.
Media, WebGL/WebGPU, popup windows, downloads, clipboard, file uploads, WebSockets,
service workers, and IME preedit are unavailable.
URLs containing embedded usernames or passwords are rejected explicitly.
Committed Unicode text is delivered to WebCore. Compatibility with general
websites therefore remains limited.

For native integration checks, start `python3 tests/webkit_server.py --port 0`
on the host. It prints the actual HTTP base URL and a QEMU guest URL. Build the
engine-only probe with the same link manifest and ordinary Rust linker:

```bash
CARGO_TARGET_AARCH64_UNKNOWN_SCARLET_LINKER=rust-lld \
cargo build --locked --release --target aarch64-unknown-scarlet \
  --no-default-features --features backend-webkit --example webkit-smoke
```

Run `webkit-smoke http://10.0.2.2:PORT` in the guest, replacing `PORT` with the
server's actual port. Use the HTTP base URL without a path.
The probe checks real HTTP/CSS/image pixels, Retina resizing, native history,
keyboard editing, POST/reload, scrolling, idle frames, and interruption of an
actual infinite script. It joins the owner after native Page destruction.
The host fixture is test input and does not substitute for a live-site GUI check.

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

Cargo features choose which engines are linked. `--backend` selects an enabled
engine for the browser window. The default desktop frontend still uses Blitz.
WebKit shutdown raises JavaScriptCore's concurrent termination trap and joins the
engine owner before native C++ global finalization. A worker that cannot finish
within 30 seconds produces an explicit failed process exit.

## License

Myrica is licensed under the [MIT License](LICENSE).
