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

Blitz remains the default engine. The `backend-webkit` and `backend-webkit-vulkan` features embed WebKit's
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

Scarlet's AArch64 desktop bundle consumes the browser and probe from the
`Scarlet prebuilt` Actions workflow. Releases are tagged
`scarlet-<full-myrica-commit>` and include a checksum-pinned `bundle.toml`.
Myrica owns the WebKit integration. Its own flake supplies the compilers and
SDK runtime used by the build.

The release includes `myrica-sources.tar.gz` with the matching Myrica source,
cached Rust crates and native dependency sources (including the modified
WebKit). License texts, copyright notices and source locations are installed
under `/share/licenses/myrica` alongside the executables. Third-party code
retains its own licenses; Myrica's independent code remains MIT licensed.

To rebuild from that archive:

```sh
tar -xf myrica-sources.tar.gz
cd myrica
nix develop
CARGO_HOME="$(cd ../cargo-home && pwd)" cargo build --locked --offline --release --target aarch64-unknown-scarlet \
  --no-default-features --features backend-webkit-vulkan,webkit-vulkan-probe \
  --bin myrica --bin webkit-vulkan-probe
```

Nix and CMake may fetch the pinned build environment and native sources. To
rebuild with a modified WebKit, first run the ordinary build above, then edit
the checkout named by
`target/aarch64-unknown-scarlet/release/build/myrica-webkit-sys-*/out/sources/webkit.path`.
The matching source is also supplied in `native/webkit-src` in the archive.
Touch `crates/myrica-webkit-sys/build.rs` and repeat the Cargo command to run
the native incremental build and relink both executables with your changes.

`myrica-webkit-sys` prepares the native installation from its Cargo build script
when `backend-webkit` is enabled. CMake FetchContent retrieves the pinned WebKit
fork and upstream font sources inside `OUT_DIR`. The WebKit fork's dependency
producer owns Cairo, ICU, Fontconfig, SQLite and their source pins. The SDK owns
libc, libc++ and libc++abi. Myrica supplies the shared-memory C ABI through its
Rust dependency, alongside its embedding adapter. No nested Cargo build or
Scarlet source checkout is required.

All engine build files, downloaded sources and installed archives remain in
Cargo's build directory. Myrica's development shell provides the SDK runtime
and raw compilers. `MYRICA_WEBKIT_LINK_MANIFEST` can explicitly
select a prebuilt installation; otherwise Cargo builds and uses its own link
manifest. `build.rs` supplies the native loader link flags for Scarlet
executables. `backend-webkit` selects the CPU Cairo profile;
`backend-webkit-vulkan` selects WebCore Skia painting through the native Vulkan
loader, SGFX ICD and VirGL backend. Both profiles build in release mode.

The AArch64 Scarlet desktop bundle selects `backend-webkit-vulkan`. WebCore
paints directly into shared BGRA GPU images. ScarletUI imports those images on
the UI thread without a CPU page readback, conversion or upload. Two leased
images provide backpressure: an image becomes reusable only after the UI has
finished sampling its replacement. Unchanged pages retain their current image.
The default Cargo features retain Blitz for desktop development and other
Scarlet architectures.

The `webkit-vulkan-probe` feature enables diagnostic GPU readback. Its
`webkit-vulkan-probe` binary is installed as `myrica-vulkan-probe` by the Scarlet
desktop recipe with the same Vulkan page profile as the browser. It checks
native C++ TLS, Rust TLS and errno isolation across four threads, then three
Ganesh create/draw/readback/destroy cycles. Its page contract exercises actual
WebCore DOM, rounded CSS, text, JavaScript and clicks at Retina scale, checks
two-image backpressure, and verifies that leased images remain unchanged.
Diagnostic readback is used only by these checks, not by browser presentation.

Run the installed binary on Scarlet:

```bash
myrica --backend webkit https://ichigo.dev/
```

To include both engines, build with the default features plus `backend-webkit`.
Select `--backend blitz` or `--backend webkit` at startup. Omitting `--backend`
keeps Blitz when it is available. The native WebKit build enables JavaScriptCore's
baseline, DFG and FTL JIT tiers, with AArch64 LLInt for cold code. Myrica's
`javascript` feature controls the Blitz/Boa engine only.

WebCore performs navigation, redirects, resource policy, HTML/CSS layout,
JavaScript, editing, and history. Myrica provides verified HTTP(S) transport.
The Vulkan profile presents shared GPU images; the CPU profile uploads
WebCore's Cairo framebuffer into its SGFX canvas. The font configuration
defaults to `/etc/fonts/fonts.conf`; `MYRICA_WEBKIT_FONT_CONFIG` can select another
installed configuration. Install its referenced fonts in the guest.

The WebKit profile supports the document Fullscreen API. A site's fullscreen
control enters native window fullscreen and hides browser chrome. Escape exits
through WebCore and restores the window. Platform state changes and declined
requests are acknowledged on the engine owner, keeping DOM fullscreen state and
the window transition synchronized.

Ganesh contexts share a device-wide resource-cache allowance derived from the
advertised Vulkan heap. Each context applies its share on its own thread; live
tiles, borrowed output images and uploads retain space outside that allowance.
Coordinated layers share native-image backing stores by image identity, matching
WebKit's LayerTreeHost, and release unused entries after owner-side layer updates.
Video texture uploads submit and retire independently of page damage, with at
most three outstanding uploads per compositor pump. An offscreen player cannot
accumulate unsent Y/UV textures and staging buffers while composition is idle.

The Scarlet WebKit profile uses WebKit storage namespaces for localStorage and
sessionStorage, a shared native CookieJarDB for HTTP and document.cookie, and
BlobRegistryImpl for object URLs. Local storage and persistent cookies use
$HOME/.local/share/myrica. Subframes retain WebCore sandbox and origin policies.
The Vulkan profile includes Scarlet's H.264/AAC MediaPlayer and MP4 Media Source
Extensions backend. Bounded CPU workers demux and decode; Skia uploads YUV planes
and performs colour conversion and page composition on the GPU. SAS supplies
audio and its consumed PCM position supplies the playback clock. The supported
profile is progressive 8-bit 4:2:0 AVC and AAC-LC mono/stereo; VP9, AV1, Opus,
HE-AAC and `avc3` are rejected. Ordinary MP4 input must fit the media core's 8 MiB
bound; MSE handles separate bounded initialization/media segments.

The native coordinated scene, Ganesh context, video-plane upload, final page
images and submission fences run on a separate compositing run loop. The UI
acquires completed shared BGRA images and returns their leases through an
independently owned mailbox, without waiting for the Page's JS/layout thread.
WebCore retains layer clipping, transforms, occlusion and damage tracking.
Vulkan contexts share the native device with externally synchronized queue
submission; each Ganesh context remains confined to its own thread. Video
presentation feedback is coalesced for the Page owner. Compressed lookahead is
bounded by 512 packets per track and the existing aggregate 8 MiB limit;
decoded output remains bounded to three video frames.
WebGL/WebGPU, popup windows, downloads, clipboard, file uploads, WebSockets,
service workers, and IME preedit are unavailable.
Identity HTTP responses deliver headers and bounded body chunks before EOF,
with bounded transport-to-Page queues. Redirects, response policies and cookies
remain in WebCore. Compressed responses still use the strict bounded whole-entity
decoder before chunk delivery. TCP window scaling is separate kernel work and
is currently absent; this transport change does not remove the 65,535-byte
advertised TCP receive-window limit.
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

`webkit-threaded-presentation`, built with `backend-webkit-vulkan`, uses
`/media/threaded` from the server's `--media-fixtures` mode. It runs a real
three-second JavaScript click handler while another thread acquires completed
GPU page images, checks continued native video presentation, joins the Page
owner, and checks that the output handle can safely outlive Page destruction.
Use `/compositor-threaded --css-only` with the same probe to check that native
CSS transform/opacity animation also advances throughout the JS handler.

Run `webkit-smoke http://10.0.2.2:PORT` in the guest, replacing `PORT` with the
server's actual port. Use the HTTP base URL without a path.
The probe checks real HTTP/CSS/image pixels, Retina resizing, native history,
keyboard editing, POST/reload, scrolling, idle frames, and interruption of an
actual infinite script. It joins the owner after native Page destruction.
The host fixture is test input and does not substitute for a live-site GUI check.

For media checks, start the fixture server with `--media-fixtures` (host FFmpeg
required), build `webkit-smoke` with `backend-webkit-vulkan`, and pass
`http://10.0.2.2:PORT --media-only` in the guest. The server generates its own
three-second H.264/AAC clip in a temporary directory and cleans it up on exit.
The probe exercises both ordinary HTML video and separate audio/video MSE
SourceBuffers, checks the actual media clock and end event, and reads real GPU
pixels to verify the decoded red, blue and green frames. Use the normal QEMU
runner with `SCARLET_QEMU_AUDIO=1` for SAS audio output.

`webkit-smoke --video-pages https://www.youtube.com/watch?v=jNQXAC9IVRw` checks
the site's actual HTML video element, delivers native page input to play it,
and requires advancing media time, real decoded/painted frames and changing
GPU pixels within the video rectangle. It keeps the site's player and source
selection intact and joins the native owner at exit.

Add `--youtube-hover --youtube-comments --youtube-search` to exercise player
hover controls, comment scrolling, continuing playback on return, and the
site's native search suggestions. Hover reports the longest observed frame
presentation gap, including owner scheduling and probe readback overhead. The
probe reports input and owner-turn times; diagnostic GPU readback makes its
frame counts unsuitable for measuring ordinary GUI playback throughput.

With `SCARLET_WEBVIEW_PROFILE=1`, slow native response-completion callbacks
report their duration, response size, content type and URL host/path. These
times include synchronous WebCore parsing and script work; they measure owner
thread blockage rather than network transfer duration. Query strings are omitted.

On a Scarlet kernel with `sync-debug`, the `scarlet_cpu_sample` example accepts
`PID [SECONDS]` and samples that process's threads through `GetTaskDebugInfo`.
It prints timer-observed instruction addresses weighted by CPU-time deltas,
alongside syscall/validity flags. Resolve addresses against the matching ELF;
shared-library addresses also require their load biases. Samples indicate
execution regions rather than precise call stacks or individual callback cost.

## Architecture

`BrowserBackend` is intentionally a WebView boundary rather than a common DOM
API. It owns navigation and history, receives viewport input, advances engine
work on one dedicated engine thread. CPU backends prepare retained SGFX meshes
there; native WebKit commits its retained scene to a separate compositor.
`BrowserFrameSource` lets the UI acquire completed native images independently
of that engine thread. The UI sends commands and takes the latest frame without
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

Scarlet Vulkan builds use WebKit’s standard threaded scrolling tree. Wheel input
can reach that tree from the UI while the document owner is busy. WebKit retains
blocking wheel-listener dispatch, with navigation and viewport checks before
accepting independent input.
