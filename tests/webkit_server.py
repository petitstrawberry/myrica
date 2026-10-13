#!/usr/bin/env python3
"""Serve real HTTP fixtures for Myrica's WebKit backend smoke tests.

The first stdout line is a JSON object with the chosen port. QEMU's user-mode
network can reach the same listener at ``http://10.0.2.2:<port>``.
"""

from __future__ import annotations

import argparse
import html
import json
import struct
import threading
import time
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit


STYLE = b"""html, body { margin: 0; padding: 0; }
body { min-height: 2400px; font-family: sans-serif; }
#header { position: absolute; left: 0; top: 0; width: 400px; height: 40px;
          background: #123456; }
#query, #notes, #one, #two, #second { position: absolute; box-sizing: border-box; }
#query { left: 20px; top: 60px; width: 300px; height: 32px; }
#notes { left: 20px; top: 120px; width: 300px; height: 80px; }
#one { left: 20px; top: 220px; width: 130px; height: 32px; }
#two { left: 170px; top: 220px; width: 130px; height: 32px; }
#second { left: 20px; top: 270px; width: 200px; height: 32px;
          display: block; line-height: 32px; }
#pixel { position: absolute; left: 400px; top: 10px; width: 16px; height: 16px; }
@media (max-width: 400px) { #header { background: #654321; } }
"""

MAIN = """<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>main-loading</title>
  <link rel="stylesheet" href="/style.css">
</head>
<body>
  <header id="header" aria-label="Color probe"></header>
  <form id="search" method="post" action="/submit">
    <input id="query" name="q" autocomplete="off" aria-label="Query">
  </form>
  <textarea id="notes" aria-label="Notes"></textarea>
  <button id="one" type="button">State one</button>
  <button id="two" type="button">State two</button>
  <a id="second" href="/second">Second page</a>
  <img id="pixel" src="/pixel.png" alt="Pixel probe">
  <script>
    function stateTitle() {
      document.title = 'state:' + (history.state?.value || 'none');
    }
    window.addEventListener('load', () => {
      document.title = history.state?.value
        ? 'state:' + history.state.value : 'main-ready';
    });
    document.getElementById('query').addEventListener('input', event => {
      document.title = 'input:' + event.target.value;
    });
    document.getElementById('query').addEventListener('keydown', event => {
      if (event.key === 'x') event.preventDefault();
    });
    document.getElementById('notes').addEventListener('input', event => {
      document.title = 'textarea:' + event.target.value.replace(/\\n/g, '\\\\n');
    });
    for (const value of ['one', 'two']) {
      document.getElementById(value).addEventListener('click', () => {
        history.pushState({value}, '', '?state=' + value);
        stateTitle();
      });
    }
    window.addEventListener('popstate', stateTitle);
    let observedWidth = window.innerWidth;
    let observedScroll = window.scrollY;
    window.addEventListener('resize', () => {
      if (window.innerWidth !== observedWidth) {
        observedWidth = window.innerWidth;
        document.title = 'width:' + observedWidth;
      }
    });
    window.addEventListener('scroll', () => {
      if (window.scrollY !== observedScroll) {
        observedScroll = window.scrollY;
        document.title = 'scroll:' + observedScroll;
      }
    });
  </script>
</body>
</html>
""".encode("utf-8")

SECOND = b"""<!doctype html><html lang="en"><head><meta charset="utf-8">
<title>second-ready</title></head><body><a href="/main">Main page</a></body></html>
"""

HUNG = b"""<!doctype html><html><head><meta charset="utf-8">
<title>hung-loading</title></head><body><script>
document.title = 'hung-entered'; while (true) {}
</script></body></html>
"""


STORAGE = b"""<!doctype html><title>storage-loading</title>
<script>
(async () => {
  try {
    localStorage.setItem('probe', 'local');
    sessionStorage.setItem('probe', 'session');
    if (localStorage.getItem('probe') !== 'local' || sessionStorage.getItem('probe') !== 'session') throw Error('storage readback');
    if (document.cookie.includes('secret=')) throw Error('HttpOnly exposed');
    if (!document.cookie.includes('network=visible')) throw Error('HTTP cookie missing');
    document.cookie = 'script=written; Path=/; SameSite=Lax';
    document.cookie = 'secret=overwritten; Path=/';
    const echo = await (await fetch('/cookie-echo', {credentials:'include'})).text();
    if (!echo.includes('network=visible') || !echo.includes('script=written') || !echo.includes('secret=hidden') || echo.includes('overwritten')) throw Error('cookie sharing: '+echo);
    const omitted = await (await fetch('/cookie-echo', {credentials:'omit'})).text();
    if (omitted !== '') throw Error('omit sent cookie');
    const blob = new Blob(['native-blob'], {type:'text/plain'});
    const url = URL.createObjectURL(blob);
    const text = await (await fetch(url)).text();
    URL.revokeObjectURL(url);
    if (text !== 'native-blob') throw Error('blob fetch');
    const frame = document.createElement('iframe');
    frame.src = '/child';
    frame.onload = () => {
      if (frame.contentDocument.body.textContent.trim() !== 'child-body') throw Error('iframe body');
      if (frame.contentWindow.sessionStorage.getItem('probe') !== 'session') throw Error('iframe session');
      document.title = 'storage-blob-iframe-ready';
    };
    document.body.appendChild(frame);
  } catch (error) { document.title = 'failed:'+error; }
})();
</script><body></body>"""

FRAMESET = b'<html><head><title>frames-loading</title></head><frameset cols="200,*"><frame src="/frame-left"><frame src="/frame-right"></frameset></html>'

def png_chunk(kind: bytes, data: bytes) -> bytes:
    """Encode a PNG chunk with its required big-endian length and CRC."""
    return (
        struct.pack("!I", len(data))
        + kind
        + data
        + struct.pack("!I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    )


def solid_png() -> bytes:
    rows = (b"\x00" + b"\xaa\xbb\xcc" * 16) * 16
    return (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", struct.pack("!IIBBBBB", 16, 16, 8, 2, 0, 0, 0))
        + png_chunk(b"IDAT", zlib.compress(rows))
        + png_chunk(b"IEND", b"")
    )


PIXEL = solid_png()


class FixtureServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address: tuple[str, int], log_path: Path | None):
        super().__init__(address, FixtureHandler)
        self.request_log = log_path.open("a", encoding="utf-8") if log_path else None
        self.log_lock = threading.Lock()

    def record_request(self, record: dict[str, object]) -> None:
        if self.request_log is None:
            return
        with self.log_lock:
            self.request_log.write(json.dumps(record, ensure_ascii=False) + "\n")
            self.request_log.flush()

    def server_close(self) -> None:
        super().server_close()
        if self.request_log is not None:
            self.request_log.close()


class FixtureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: FixtureServer

    def log_message(self, format: str, *args: object) -> None:
        # Keep stdout reserved for machine-readable startup information.
        pass

    def reply(
        self,
        status: int,
        content_type: str,
        body: bytes,
        extra_headers: tuple[tuple[str, str], ...] = (),
    ) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        for name, value in extra_headers:
            self.send_header(name, value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def handle_fixture(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length) if length else b""
        path = urlsplit(self.path).path
        record: dict[str, object] = {
            "time_ns": time.time_ns(),
            "method": self.command,
            "target": self.path,
            "content_type": self.headers.get("Content-Type"),
            "body": body.decode("utf-8", errors="replace"),
        }
        self.server.record_request(record)

        if path == "/submit" and self.command == "POST":
            query = parse_qs(body.decode("utf-8"), keep_blank_values=True).get("q", [""])[0]
            title = html.escape("posted:" + query, quote=True)
            response = (
                '<!doctype html><html lang="en"><head><meta charset="utf-8">'
                f"<title>{title}</title></head><body>"
                f'<p id="posted">{html.escape(query)}</p><a href="/main">Main page</a>'
                "</body></html>"
            ).encode("utf-8")
            self.reply(200, "text/html; charset=utf-8", response)
        elif self.command not in ("GET", "HEAD"):
            self.reply(405, "text/plain; charset=utf-8", b"Method not allowed\n", (("Allow", "GET, HEAD"),))
        elif path in ("/", "/main"):
            self.reply(200, "text/html; charset=utf-8", MAIN)
        elif path == "/second":
            self.reply(200, "text/html; charset=utf-8", SECOND)
        elif path == "/storage":
            self.reply(200, "text/html; charset=utf-8", STORAGE, (("Set-Cookie", "network=visible; Path=/; SameSite=Lax"), ("Set-Cookie", "secret=hidden; HttpOnly; Path=/; SameSite=Lax")))
        elif path == "/cookie-echo":
            self.reply(200, "text/plain", self.headers.get("Cookie", "").encode())
        elif path == "/child":
            self.reply(200, "text/html", b"<html><body style='margin:0;background:#2468ac'>child-body</body></html>")
        elif path == "/frames":
            self.reply(200, "text/html", FRAMESET)
        elif path == "/frame-left":
            self.reply(200, "text/html", b"<html><body style='margin:0;background:#2468ac'>left</body></html>")
        elif path == "/frame-right":
            self.reply(200, "text/html", b"<html><body style='margin:0;background:#ac6824'>right<script>window.addEventListener('load',()=>parent.document.title='frames-ready')</script></body></html>")
        elif path == "/hung":
            self.reply(200, "text/html; charset=utf-8", HUNG)
        elif path == "/style.css":
            self.reply(200, "text/css; charset=utf-8", STYLE)
        elif path == "/pixel.png":
            self.reply(200, "image/png", PIXEL)
        elif path == "/redirect":
            self.reply(302, "text/plain; charset=utf-8", b"Redirecting\n", (("Location", "/main"),))
        else:
            self.reply(404, "text/plain; charset=utf-8", b"Not found\n")

    do_GET = handle_fixture
    do_HEAD = handle_fixture
    do_POST = handle_fixture


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=0, help="Listen port (0 chooses a free port)")
    parser.add_argument("--log", type=Path, help="Append actual requests to this JSONL file")
    args = parser.parse_args()
    if not 0 <= args.port <= 65535:
        parser.error("--port must be between 0 and 65535")
    with FixtureServer(("127.0.0.1", args.port), args.log) as server:
        port = server.server_address[1]
        print(json.dumps({
            "host": "127.0.0.1",
            "port": port,
            "base_url": f"http://127.0.0.1:{port}",
            "guest_url": f"http://10.0.2.2:{port}",
        }), flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
