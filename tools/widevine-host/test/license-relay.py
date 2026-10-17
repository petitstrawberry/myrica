#!/usr/bin/env python3
# Minimal HTTPS relay for the Widevine test licence proxy.
#
# The container-side test client has no TLS stack, so it POSTs the licence
# request to this relay over plain HTTP; the relay forwards the bytes to the
# Widevine test proxy over HTTPS and returns the response verbatim.
#
# usage: license-relay.py [PORT] [LICENSE_URL]

import http.server
import sys
import urllib.request

LICENSE_URL = "https://cwip-shaka-proxy.appspot.com/no_auth"


class Relay(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        request = urllib.request.Request(LICENSE_URL, data=body, method="POST")
        request.add_header("Content-Type", "application/octet-stream")
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                data = response.read()
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            print(f"[relay] forwarded {len(body)} bytes, got {len(data)} bytes", flush=True)
        except Exception as error:
            self.send_response(502)
            self.end_headers()
            self.wfile.write(str(error).encode())
            print(f"[relay] upstream failure: {error}", flush=True)

    def log_message(self, format, *args):
        print("[relay] " + (format % args), flush=True)


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8788
    global LICENSE_URL
    if len(sys.argv) > 2:
        LICENSE_URL = sys.argv[2]
    print(f"[relay] listening on {port}, upstream {LICENSE_URL}", flush=True)
    http.server.ThreadingHTTPServer(("0.0.0.0", port), Relay).serve_forever()


if __name__ == "__main__":
    main()
