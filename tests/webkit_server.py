#!/usr/bin/env python3
"""Serve real HTTP fixtures for Myrica's WebKit backend smoke tests.

The first stdout line is a JSON object with the chosen port. QEMU's user-mode
network can reach the same listener at ``http://10.0.2.2:<port>``.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import html
import json
import struct
import subprocess
import tempfile
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

VIEW_TRANSITION = b"""<!doctype html><meta charset="utf-8"><title>view-transition-ready</title>
<style>body{margin:0}#patch{position:absolute;left:20px;top:20px;width:128px;height:96px;background:#123456}
button{position:absolute;left:20px;top:140px;width:128px;height:32px}</style>
<div id="patch"></div><button id="transition">Update</button>
<span id="view-transition-state" style="display:none"></span>
<script>
let calls = 0;
let callbackAsync = false;
let ready = 'pending';
let readyError = null;
let updateCallbackDone = 'pending';
let finished = 'pending';
function publishTransitionState() {
  document.getElementById('view-transition-state').textContent = JSON.stringify({
    calls, callbackAsync, ready, readyError, updateCallbackDone, finished,
    active: document.activeViewTransition === null ? null : 'not-null'
  });
}
document.getElementById('transition').addEventListener('click', async () => {
  try {
    let insideStart = true;
    const transition = document.startViewTransition(() => {
      calls++;
      callbackAsync = !insideStart;
      document.getElementById('patch').style.backgroundColor = '#654321';
      publishTransitionState();
    });
    insideStart = false;
    await Promise.all([
      transition.ready.then(() => { ready = 'fulfilled'; }, error => {
        ready = 'rejected'; readyError = error.name;
      }),
      transition.updateCallbackDone.then(() => { updateCallbackDone = 'fulfilled'; }),
      transition.finished.then(() => { finished = 'fulfilled'; })
    ]);
    publishTransitionState();
    document.title = 'view-transition-done';
  } catch (error) {
    document.title = 'failed:' + error;
  }
});
publishTransitionState();
</script>"""

JS_EXECUTION = b"""<!doctype html><meta charset="utf-8"><title>js-execution-ready</title>
<style>body{margin:0}#patch{position:absolute;left:20px;top:20px;width:128px;height:96px;background:#123456}
button{position:absolute;left:20px;top:140px;width:128px;height:32px}</style>
<div id="patch"></div><button id="execute">Run contract</button>
<span id="js-execution-state" style="display:none"></span>
<script>
let clicks = 0;
document.getElementById('execute').addEventListener('click', async () => {
  clicks++;
  try {
    function sum(a, b) { return (a ?? 0) + (b ?? 0); }
    function arity(a, b) { return [a, typeof b, arguments.length]; }
    function factorial(n) { return n < 2 ? 1 : n * factorial(n - 1); }
    function counter(value) { return (step = 1) => value += step; }
    function invoke(fn, ...args) { return fn(...args); }
    const count = counter(40);
    function Box(value) { this.value = value; }
    Box.prototype.twice = function() { return this.value * 2; };
    const box = new Box(21);
    const child = Object.create(box);
    child.value = 10;
    const result = {
      calls: [sum(4, 5), sum(4), sum(4, 5, 99), sum.apply(null, [6, 7, 8]), Math.imul(7, 9)],
      arity: [arity(7), arity(7, 8, 9)],
      recursion: factorial(7),
      closure: [count(2), count(), invoke(count, 7)],
      prototype: [box.twice(), child.twice(), Object.getPrototypeOf(box) === Box.prototype]
    };
    const exceptions = [];
    function throwFromCall() { throw new Error('js-sentinel'); }
    try { throwFromCall(); }
    catch (error) { exceptions.push(error.message); }
    finally { exceptions.push('finally-js'); }
    try { document.querySelector('['); }
    catch (error) { exceptions.push(error.name); }
    finally { exceptions.push('finally-dom'); }
    result.exceptions = exceptions;
    const bytes = new Uint8Array([3, 5, 7, 11]);
    new DataView(bytes.buffer).setUint8(1, 9);
    result.typedArray = [bytes.reduce((a, b) => a + b, 0), ...bytes.subarray(1, 3)];
    const rooted = {text: 'root-live', values: [5, 8, 13], check: () => box.twice()};
    const map = new Map([[rooted, {value: 13}], ['count', 2]]);
    let allocationChecksum = 0;
    let batches = 0;
    // Bounded, short-lived allocations only: retain the closure/object/Map
    // roots across calls and microtask suspension without a nonstandard gc().
    for (let i = 0; i < 1024; i++) {
      const temporary = Array.from({length: 8}, (_, j) => ({value: i + j, owner: rooted}));
      allocationChecksum += temporary[0].value;
      batches++;
      if ((i & 63) === 63) await Promise.resolve();
    }
    result.allocation = [allocationChecksum, batches];
    result.rooted = [rooted.text, rooted.values.reduce((a, b) => a + b, 0), rooted.check(), map.has(rooted)];
    result.map = [map.get(rooted).value, map.get('count'), map.size];
    const order = ['sync'];
    const pending = Promise.resolve().then(() => { order.push('microtask'); return 17; });
    order.push('after-schedule');
    result.promise = await pending;
    order.push('await');
    await new Promise(resolve => setTimeout(() => { order.push('timer'); resolve(); }, 0));
    result.order = order;
    let domCallbacks = 0;
    let detail = 0;
    const patch = document.getElementById('patch');
    patch.addEventListener('contract-update', event => {
      domCallbacks++;
      detail = event.detail;
      patch.style.backgroundColor = '#654321';
    }, {once: true});
    patch.dispatchEvent(new CustomEvent('contract-update', {detail: result.promise}));
    result.dom = [clicks, domCallbacks, detail];
    document.getElementById('js-execution-state').textContent = JSON.stringify(result);
    document.title = 'js-execution-done';
  } catch (error) {
    document.getElementById('js-execution-state').textContent = JSON.stringify({error: String(error)});
    document.title = 'js-execution-failed:' + error;
  }
});
</script>"""

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
    const opened = indexedDB.open('myrica-probe', 1);
    opened.onupgradeneeded = () => opened.result.createObjectStore('records');
    const db = await new Promise((resolve, reject) => {
      opened.onsuccess = () => resolve(opened.result);
      opened.onerror = () => reject(opened.error);
    });
    const written = db.transaction('records', 'readwrite');
    written.objectStore('records').put('indexed-value', 'probe');
    await new Promise((resolve, reject) => {
      written.oncomplete = resolve;
      written.onabort = () => reject(written.error);
    });
    const read = db.transaction('records').objectStore('records').get('probe');
    const value = await new Promise((resolve, reject) => {
      read.onsuccess = () => resolve(read.result);
      read.onerror = () => reject(read.error);
    });
    db.close();
    if (value !== 'indexed-value') throw Error('IndexedDB transaction readback');
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


def solid_png(width: int = 16, height: int = 16, colour: bytes = b"\xaa\xbb\xcc") -> bytes:
    rows = (b"\x00" + colour * width) * height
    return (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", struct.pack("!IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + png_chunk(b"IDAT", zlib.compress(rows))
        + png_chunk(b"IEND", b"")
    )


PIXEL = solid_png()


# Self-generated codec fixtures: FFmpeg mjpeg q=2/yuvj444p and libwebp
# lossless=1 from solid RGB PPM input. Embedded bytes keep the normal HTTP
# fixture independent of host codec tools; native WebCore still decodes them.
IMAGE_RESOURCES = {
    "/images/red.png": ("image/png", solid_png(64, 48, bytes((208, 32, 48)))),
    "/images/green.jpg": ("image/jpeg", base64.b64decode(
        b"/9j//gAQTGF2YzYyLjI4LjEwMgD/2wBDAAgEBAQEBAUFBQUFBQYGBgYGBgYGBgYGBgYHBwcICAgHBwcGBgcHCAgICAkJCQgI"
        b"CAgJCQoKCgwMCwsODg4RERT/xABNAAEBAAAAAAAAAAAAAAAAAAAAAgEBAQEAAAAAAAAAAAAAAAAAAAYHEAEAAAAAAAAAAAAA"
        b"AAAAAAAAEQEAAAAAAAAAAAAAAAAAAAAA/8AAEQgAMABAAwESAAISAAMSAP/aAAwDAQACEQMRAD8AoTQzkAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAf/9k="
    )),
    "/images/blue.webp": ("image/webp", base64.b64decode(
        b"UklGRiQAAABXRUJQVlA4TBcAAAAvP8ALAAdQqMIUuv8BICH8Xy9G9D/1AwA="
    )),
    "/images/quadrants.jpg": ("image/jpeg", base64.b64decode(
        b"/9j//gAQTGF2YzYyLjI4LjEwMgD/2wBDAAgEBAQEBAUFBQUFBQYGBgYGBgYGBgYGBgYHBwcICAgHBwcGBgcHCAgICAkJCQgI"
        b"CAgJCQoKCgwMCwsODg4RERT/xABSAAEBAQEAAAAAAAAAAAAAAAAABgEHAQEBAQEBAQAAAAAAAAAAAAAABwYIBAUQAQAAAAAA"
        b"AAAAAAAAAAAAAAARAQAAAAAAAAAAAAAAAAAAAAD/wAARCALQBQADARIAAhIAAxIA/9oADAMBAAIRAxEAPwCVHyRVgAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABWDwDmMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAASY946cAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAF"
        b"YPAOYwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABJj3jpwAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAVg8A5jAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAEmPeOnAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABWDwDmMAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAASY946cAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAFYPAOYwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABJj"
        b"3jpwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAVg8A5jAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEmPeOnAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAABWDwDmMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAASY946cAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAFYPAOYwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAABJj3jpwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAVg8A"
        b"5jAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEmPeOnAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABWDwDmMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAASY946cAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAFYPAOYwAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABJj3jpwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAVg8A5jAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEmPeOn"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABWDwDmMAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAASY946cAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAFYPAOYwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABJj3jpwAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAVg8A5jAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAEmPeOnAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABWDwDmMA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAASY946cAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAFYPAOYwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAABJj3jpwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAVg8A5jAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEmPeOnAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAABWDwDmMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAASY946cAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAFYPAOYwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABJj3jpwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAVg8A5jAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEmPeOnAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABWDwDmMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAASbHvVt04hzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYk"
        b"itrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uK"
        b"HNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iS"
        b"K2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4o"
        b"c1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJI"
        b"ra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrih"
        b"zWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYkitrihzWJIra4oc1iSK2uKHNYki"
        b"trihzWJIra4ocrVqirHMosSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatix"
        b"yOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2L"
        b"HI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrY"
        b"scjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpat"
        b"ixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq"
        b"2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKW"
        b"rYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIp"
        b"atixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEilq2LHI6sSKWrYscjqxIpatixyOrEi"
        b"lq2LHI6sSKWrYscjqxOOjpsYgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAdiHMg24AA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA46OmxiAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB2IcyDbgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAADjo6bGIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHYhzINuAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAOOjpsYgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAdiHMg24AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA46OmxiA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB2IcyDbgAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADjo6bGIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAHYhzINuAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAOOjpsYgAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAdiHMg24AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAA46OmxiAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB2IcyD"
        b"bgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADjo6bGIAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHYhzINuAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAOOjpsYgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAdiHMg24AAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA46OmxiAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAB2IcyDbgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADjo6"
        b"bGIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHYhzINuAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAOOjpsYgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAdiHMg24AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA46OmxiAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB2IcyDbgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAADjo6bGIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHY"
        b"hzINuAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAOOjpsYgAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAdiHMg24AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAA46OmxiAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB2IcyDbgAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADjo6bGIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAHYhzINuAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"OOjpsYgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAdiHMg24AAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA46OmxiAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAB2IcyDbgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADjo6bGIAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAHYhzINuAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAOOjpsYgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAdiHMg24AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA//9k="
    )),
}


IMAGE_PAGE = b"""<!doctype html><meta charset="utf-8"><title>images-loading</title>
<style>
html,body{margin:0;background:white;width:800px;min-height:1500px}
img{position:absolute;width:64px;height:48px}#board{position:absolute;left:20px;top:20px}
.png{left:0}.jpeg{left:80px}.webp{left:160px}.flat{top:0}.layer{top:72px;transform:translateZ(0);will-change:transform}
#large-image{left:0;top:160px;width:320px;height:180px}
#async-image{left:260px;top:0}#cached{position:absolute;left:20px;top:800px}#cached img{top:0}
#load-image,#move-images{position:fixed;left:500px;top:20px;width:160px;height:36px;background:#ddd}
#move-images{top:80px}
</style>
<div id="board">
<img id="flat-png" class="flat png" src="/images/red.png">
<img id="flat-jpeg" class="flat jpeg" src="/images/green.jpg">
<img id="flat-webp" class="flat webp" src="/images/blue.webp">
<img id="layer-png" class="layer png" src="/images/red.png">
<img id="layer-jpeg" class="layer jpeg" src="/images/green.jpg">
<img id="layer-webp" class="layer webp" src="/images/blue.webp">
<img id="large-image" src="/images/quadrants.jpg">
<img id="async-image" src="/images/red.png">
</div>
<div id="cached">
<img id="cached-png" class="png" src="/images/red.png">
<img id="cached-jpeg" class="jpeg" src="/images/green.jpg">
<img id="cached-webp" class="webp" src="/images/blue.webp">
</div>
<div id="load-image">Load new image</div><div id="move-images">Move image layers</div>
<span id="images-state" style="display:none"></span>
<script>
const decoded = new Set();
let asyncLoadEvents = 0;
let moved = false;
function publishImages() {
  document.getElementById('images-state').textContent = JSON.stringify({
    asyncLoadEvents, moved, scrollY,
    images: Array.from(document.images, image => ({
      id: image.id, complete: image.complete, width: image.naturalWidth,
      height: image.naturalHeight, decoded: decoded.has(image.id),
      path: new URL(image.currentSrc || image.src).pathname
    }))
  });
}
function failImage(error) { document.title = 'images-error:' + error; publishImages(); }
Promise.all(Array.from(document.images, async image => {
  await image.decode();
  decoded.add(image.id);
})).then(() => { publishImages(); document.title = 'images-ready'; }).catch(failImage);
document.getElementById('load-image').addEventListener('click', async () => {
  try {
    const image = document.getElementById('async-image');
    decoded.delete(image.id);
    const loaded = new Promise((resolve, reject) => {
      image.addEventListener('load', () => { asyncLoadEvents++; resolve(); }, {once:true});
      image.addEventListener('error', () => reject(Error('async image load')), {once:true});
    });
    image.src = '/images/blue.webp?stage=async';
    await loaded;
    await image.decode();
    decoded.add(image.id);
    publishImages(); document.title = 'images-async';
  } catch (error) { failImage(error); }
});
document.getElementById('move-images').addEventListener('click', () => {
  for (const image of document.querySelectorAll('.layer')) image.style.transform = 'translate3d(24px,0,0)';
  moved = true; publishImages(); document.title = 'images-transformed';
});
window.addEventListener('scroll', () => { publishImages(); document.title = 'images-scroll:' + scrollY; });
</script>"""


MEDIA_PAGE = b"""<!doctype html><meta charset="utf-8"><title>media-loading</title>
<style>body{margin:0}video{position:absolute;left:20px;top:20px;width:128px;height:96px}
button{position:absolute;left:20px;top:140px;width:128px;height:32px}</style>
<video id="video" playsinline preload="auto"></video><button id="play">Play</button>
<span id="media-state" style="display:none"></span>
<script>
const video = document.getElementById('video');
let frameCallbacks = 0;
let lastFrameMetadata = null;
let observingFrames = false;
let playClicks = 0;
let presentationEpoch = 0;
const frameHistory = [];
let encodingEvidence = null;
let sniffReloadEvidence = null;
const mse = location.pathname.endsWith('/mse') || location.pathname.endsWith('/mse-big');
function fail(error) { document.title = 'media-error:' + String(error); }
video.addEventListener('error', () => fail('video:' + video.error?.code));
video.addEventListener('canplay', () => {
  updateState();
  if (video.paused) document.title = 'media-ready';
  if (location.pathname.endsWith('/normal-sniff-reload') && !sniffReloadEvidence) {
    fetch('/media/sniff-reload-state.json').then(response => response.json())
      .then(evidence => { sniffReloadEvidence = evidence; updateState(); }).catch(fail);
  }
});
video.addEventListener('timeupdate', () => {
  if (!video.paused) document.title = 'media-time:' + video.currentTime.toFixed(3);
});
video.addEventListener('ended', () => { document.title = 'media-ended:' + video.currentTime.toFixed(3); });
document.getElementById('play').addEventListener('click', () => {
  ++playClicks;
  if (!observingFrames) {
    observingFrames = true;
    function frameReady(now, metadata) {
      ++frameCallbacks;
      lastFrameMetadata = { time:metadata.mediaTime, count:metadata.presentedFrames,
                            width:metadata.width, height:metadata.height };
      frameHistory.push({...lastFrameMetadata, epoch:presentationEpoch});
      if (frameHistory.length > 128) frameHistory.shift();
      video.requestVideoFrameCallback(frameReady);
    }
    video.requestVideoFrameCallback(frameReady);
  }
  video.play().catch(fail);
});
function updateState() {
  document.getElementById('media-state').textContent = JSON.stringify({
    time:video.currentTime, duration:video.duration, paused:video.paused,
    ended:video.ended, ready:video.readyState, width:video.videoWidth, height:video.videoHeight,
    error:video.error?.code || 0, callbacks:frameCallbacks, metadata:lastFrameMetadata,
    quality:video.getVideoPlaybackQuality().totalVideoFrames,
    frames:frameHistory, encoding:encodingEvidence, playClicks, sniffReload:sniffReloadEvidence
  });
}
updateState();
setInterval(updateState, 40);
window.addEventListener('keydown', event => {
  if (event.key === 'p') {
    if (video.paused) video.play().catch(fail);
    else { video.pause(); document.title = 'media-paused'; }
  } else if (event.key === 's') {
    ++presentationEpoch;
    video.currentTime = 0.25;
  }
});
async function append(buffer, bytes) {
  await new Promise((resolve, reject) => {
    buffer.addEventListener('updateend', resolve, {once:true});
    buffer.addEventListener('error', reject, {once:true});
    buffer.appendBuffer(bytes);
  });
}
async function loadMse() {
  const source = new MediaSource();
  video.src = URL.createObjectURL(source);
  await new Promise(resolve => source.addEventListener('sourceopen', resolve, {once:true}));
  const manifest = location.pathname.endsWith('/mse-big') ? '/media/manifest-big.json' : '/media/manifest.json';
  const tracks = await (await fetch(manifest)).json();
  // Like YouTube, video and audio use separate SourceBuffers, real codec
  // strings and independent init/media segments. No player DOM substitution.
  await Promise.all(tracks.map(async track => {
    if (!MediaSource.isTypeSupported(track.type)) throw Error('unsupported:' + track.type);
    const buffer = source.addSourceBuffer(track.type);
    for (const url of track.segments) await append(buffer, await (await fetch(url)).arrayBuffer());
  }));
  source.endOfStream();
}
const fixture = mse ? 'normal' : location.pathname.split('/').pop();
const encodingFixture = fixture.startsWith('normal-sniff') ? 'normal' : fixture;
fetch('/media/' + encodingFixture + '.encoding.json')
  .then(response => response.json())
  .then(evidence => { encodingEvidence = evidence; updateState(); }).catch(fail);
if (mse) loadMse().catch(fail);
else if (fixture === 'normal-sniff') video.src = '/media/normal-sniff-data';
else if (fixture === 'normal-sniff-reload') video.src = '/media/normal-sniff-reload-data';
else video.src = '/media/' + fixture + '.mp4';
</script>"""


# The video has its own accelerated layer; the board remains genuine static DOM
# content. Frame callbacks/state update only a hidden span and cannot dirty it.
COMPOSITOR_MEDIA_PAGE = MEDIA_PAGE.replace(
    b"<video id=", b'<div id="static-board"></div><video id=', 1,
).replace(
    b"</style>",
    b"body{background:#2468ac}#static-board{position:absolute;left:200px;top:20px;"
    b"width:160px;height:180px;background:#123456}video{transform:translateZ(0);"
    b"will-change:transform}</style>", 1,
).replace(
    b"const fixture = mse ? 'normal' : location.pathname.split('/').pop();",
    b"const fixture = 'normal';", 1,
)


def media_segments(data: bytes) -> list[bytes]:
    """Split FFmpeg's fragmented MP4 into init and complete moof/mdat pairs."""
    segments: list[bytes] = []
    start = cursor = 0
    while cursor < len(data):
        if len(data) - cursor < 8:
            raise ValueError("truncated generated MP4 box")
        size, kind = struct.unpack_from(">I4s", data, cursor)
        if size == 1:
            size = struct.unpack_from(">Q", data, cursor + 8)[0]
        elif size == 0:
            size = len(data) - cursor
        if size < 8 or cursor + size > len(data):
            raise ValueError("invalid generated MP4 box")
        if kind == b"moof" and cursor > start:
            segments.append(data[start:cursor])
            start = cursor
        cursor += size
    if start < cursor:
        segments.append(data[start:cursor])
    return segments


def video_encoding_evidence(path: Path, expected_profile: str, reordered: bool) -> dict:
    """Record actual coded/display order; reject duped CFR or B-frame-free inputs."""
    def probe(kind: str, entries: str) -> dict:
        output = subprocess.check_output([
            "ffprobe", "-v", "error", "-select_streams", "v:0", f"-show_{kind}",
            "-show_entries", entries, "-of", "json", str(path),
        ])
        return json.loads(output)

    stream = probe("streams", "stream=codec_name,profile,width,height,has_b_frames,r_frame_rate")['streams'][0]
    frames = probe("frames", "frame=best_effort_timestamp_time,pict_type")['frames']
    packets = probe("packets", "packet=pts_time,dts_time,duration_time")['packets']
    pts = [float(frame['best_effort_timestamp_time']) for frame in frames]
    packet_pts = [float(packet['pts_time']) for packet in packets]
    packet_dts = [float(packet['dts_time']) for packet in packets]
    expected_pts = [index / 8 for index in range(24)]
    b_frames = sum(frame['pict_type'] == 'B' for frame in frames)
    has_reorder = any(right < left for left, right in zip(packet_pts, packet_pts[1:]))
    if (stream['codec_name'] != 'h264' or stream['profile'] != expected_profile
            or stream['width'] != 128 or stream['height'] != 96 or stream['r_frame_rate'] != '8/1'
            or len(frames) != 24 or len(packets) != 24 or pts != expected_pts
            or sorted(packet_pts) != expected_pts
            or any(right <= left for left, right in zip(packet_dts, packet_dts[1:]))
            or bool(b_frames) != reordered or has_reorder != reordered
            or bool(stream['has_b_frames']) != reordered):
        raise ValueError(f"unexpected actual H.264 fixture encoding: {path.name}: {stream}, "
                         f"frames={len(frames)} B={b_frames} PTS={pts} packetPTS={packet_pts}")
    return {
        "profile": stream['profile'], "frames": len(frames), "width": stream['width'],
        "height": stream['height'], "b_frames": b_frames, "reorder_depth": stream['has_b_frames'],
        "pts": pts, "packet_pts": packet_pts, "packet_dts": packet_dts,
    }


def generate_media(directory: Path) -> dict[str, tuple[str, bytes]]:
    """Create our own three-second H.264/AAC fixtures; nothing is downloaded."""
    video = directory / "video.mp4"
    audio = directory / "audio.mp4"
    normal = directory / "normal.mp4"
    common = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-threads", "1"]
    subprocess.run(common + [
        "-f", "lavfi", "-i", "color=red:s=128x96:r=8:d=1",
        "-f", "lavfi", "-i", "color=blue:s=128x96:r=8:d=1",
        "-f", "lavfi", "-i", "color=lime:s=128x96:r=8:d=1",
        "-filter_complex", "[0:v][1:v][2:v]concat=n=3:v=1:a=0,scale=out_color_matrix=bt709:out_range=tv[v]",
        "-map", "[v]", "-c:v", "libx264", "-profile:v", "baseline", "-level:v", "3.0",
        "-pix_fmt", "yuv420p", "-g", "8", "-keyint_min", "8", "-sc_threshold", "0", "-bf", "0",
        "-fps_mode", "passthrough", "-enc_time_base", "1:8", "-frames:v", "24",
        "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv",
        "-movflags", "empty_moov+frag_keyframe+default_base_moof", str(video),
    ], check=True)
    subprocess.run(common + [
        "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=3",
        "-ac", "2", "-c:a", "aac", "-profile:a", "aac_low", "-b:a", "64k",
        "-movflags", "empty_moov+frag_keyframe+default_base_moof", "-frag_duration", "1000000", str(audio),
    ], check=True)
    subprocess.run(common + ["-i", str(video), "-i", str(audio), "-map", "0:v", "-map", "1:a",
                             "-c", "copy", "-movflags", "+faststart", str(normal)], check=True)
    resources = {"/media/normal.mp4": ("video/mp4", normal.read_bytes())}
    # No filename extension or <source type> can supply the initial container.
    # The ordinary opaque-MIME path is separate from the reload regression:
    # Scarlet accepts opaque MP4 directly, without MediaResourceSniffer.
    for url in ("/media/normal-sniff-data", "/media/normal-sniff-reload-data"):
        resources[url] = ("application/octet-stream", resources["/media/normal.mp4"][1])
    evidence = video_encoding_evidence(normal, "Constrained Baseline", False)
    resources["/media/normal.encoding.json"] = ("application/json", json.dumps(evidence).encode())
    for profile in ("main", "high"):
        encoded = directory / f"video-{profile}.mp4"
        muxed = directory / f"normal-{profile}.mp4"
        # Passthrough emits each of the 24 source frames exactly once. Fixed
        # B-adapt=0 forces real B pictures even for these simple solid colours.
        subprocess.run(common + [
            "-f", "lavfi", "-i", "color=red:s=128x96:r=8:d=1",
            "-f", "lavfi", "-i", "color=blue:s=128x96:r=8:d=1",
            "-f", "lavfi", "-i", "color=lime:s=128x96:r=8:d=1",
            "-filter_complex", "[0:v][1:v][2:v]concat=n=3:v=1:a=0,scale=out_color_matrix=bt709:out_range=tv[v]",
            "-map", "[v]", "-c:v", "libx264", "-profile:v", profile, "-level:v", "3.0",
            "-pix_fmt", "yuv420p", "-g", "8", "-keyint_min", "8", "-sc_threshold", "0",
            "-bf", "2", "-x264-params", "b-adapt=0", "-threads", "1",
            "-fps_mode", "passthrough", "-enc_time_base", "1:8", "-frames:v", "24",
            "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv",
            "-movflags", "+faststart", str(encoded),
        ], check=True)
        subprocess.run(common + ["-i", str(encoded), "-i", str(audio), "-map", "0:v", "-map", "1:a",
                                 "-c", "copy", "-movflags", "+faststart", str(muxed)], check=True)
        resources[f"/media/normal-{profile}.mp4"] = ("video/mp4", muxed.read_bytes())
        evidence = video_encoding_evidence(muxed, profile.capitalize(), True)
        resources[f"/media/normal-{profile}.encoding.json"] = ("application/json", json.dumps(evidence).encode())
    tracks = []
    for name, path, media_type in [
        ("video", video, 'video/mp4; codecs="avc1.42C01E"'),
        ("audio", audio, 'audio/mp4; codecs="mp4a.40.2"'),
    ]:
        urls = []
        for index, segment in enumerate(media_segments(path.read_bytes())):
            url = f"/media/{name}-{index}.mp4"
            resources[url] = (f"{name}/mp4", segment)
            urls.append(url)
        tracks.append({"type": media_type, "segments": urls})
    resources["/media/manifest.json"] = ("application/json", json.dumps(tracks).encode())
    # A legal ignored top-level free box makes the init append cross several
    # C ABI chunks. This catches truncated append promises and size mismatches
    # without requiring an expensive high-resolution decode fixture.
    big_init = "/media/video-0-big.mp4"
    resources[big_init] = ("video/mp4", resources[tracks[0]["segments"][0]][1]
                          + struct.pack(">I4s", 2 * 1024 * 1024 + 8, b"free") + bytes(2 * 1024 * 1024))
    big_tracks = json.loads(json.dumps(tracks))
    big_tracks[0]["segments"][0] = big_init
    resources["/media/manifest-big.json"] = ("application/json", json.dumps(big_tracks).encode())
    return resources


class FixtureServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address: tuple[str, int], log_path: Path | None,
                 media: dict[str, tuple[str, bytes]] | None = None):
        super().__init__(address, FixtureHandler)
        self.request_log = log_path.open("a", encoding="utf-8") if log_path else None
        self.log_lock = threading.Lock()
        self.media = media or {}
        self.sniff_reload_lock = threading.Lock()
        self.sniff_reload_requests: list[dict[str, object]] = []

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
        cache_control: str = "no-store",
    ) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", cache_control)
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
        elif path == "/media/sniff-reload-state.json" and self.server.media:
            with self.server.sniff_reload_lock:
                observation = {"requests": list(self.server.sniff_reload_requests)}
            self.reply(200, "application/json", json.dumps(observation).encode())
        elif path in self.server.media:
            content_type, content = self.server.media[path]
            range_header = self.headers.get("Range")
            if path == "/media/normal-sniff-reload-data":
                with self.server.sniff_reload_lock:
                    # A genuinely misdeclared first load must fail format
                    # selection. Sniffer and retry receive the same MP4 bytes
                    # with an opaque MIME, exercising ordinary HTTP reload.
                    if not self.server.sniff_reload_requests:
                        content_type = "video/webm"
                    self.server.sniff_reload_requests.append({
                        "method": self.command, "range": range_header,
                        "content_type": content_type,
                    })
            if range_header and range_header.startswith("bytes="):
                try:
                    first, last = range_header[6:].split("-", 1)
                    start = int(first) if first else max(0, len(content) - int(last))
                    end = min(int(last), len(content) - 1) if last and first else len(content) - 1
                    if not 0 <= start <= end < len(content):
                        raise ValueError("invalid range")
                except ValueError:
                    self.reply(416, content_type, b"", (("Content-Range", f"bytes */{len(content)}"),))
                    return
                self.reply(206, content_type, content[start:end + 1],
                           (("Accept-Ranges", "bytes"), ("Content-Range", f"bytes {start}-{end}/{len(content)}")))
            else:
                self.reply(200, content_type, content, (("Accept-Ranges", "bytes"),))
        elif path == "/media/compositor" and self.server.media:
            self.reply(200, "text/html; charset=utf-8", COMPOSITOR_MEDIA_PAGE)
        elif path in ("/media/normal", "/media/normal-main", "/media/normal-high", "/media/normal-sniff", "/media/normal-sniff-reload", "/media/mse", "/media/mse-big") and self.server.media:
            if path == "/media/normal-sniff-reload":
                with self.server.sniff_reload_lock:
                    self.server.sniff_reload_requests.clear()
            self.reply(200, "text/html; charset=utf-8", MEDIA_PAGE)
        elif path in ("/", "/main"):
            self.reply(200, "text/html; charset=utf-8", MAIN)
        elif path == "/images":
            self.reply(200, "text/html; charset=utf-8", IMAGE_PAGE)
        elif path in IMAGE_RESOURCES:
            content_type, content = IMAGE_RESOURCES[path]
            self.reply(200, content_type, content, cache_control="public, max-age=3600")
        elif path == "/second":
            self.reply(200, "text/html; charset=utf-8", SECOND)
        elif path == "/view-transition":
            self.reply(200, "text/html; charset=utf-8", VIEW_TRANSITION)
        elif path == "/js-execution":
            self.reply(200, "text/html; charset=utf-8", JS_EXECUTION)
        elif path == "/damage":
            self.reply(200, "text/html", b"<!doctype html><title>damage-ready</title><body style='margin:0'><div style='position:absolute;left:20px;top:20px;width:20px;height:20px;background:#123456' id='patch'></div><script>window.addEventListener('keydown',()=>{document.getElementById('patch').style.backgroundColor='#654321';document.title='damage-changed'})</script></body>")
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
    parser.add_argument("--media-fixtures", action="store_true", help="Generate H.264/AAC and MSE fixtures using host FFmpeg")
    args = parser.parse_args()
    if not 0 <= args.port <= 65535:
        parser.error("--port must be between 0 and 65535")
    with contextlib.ExitStack() as stack:
        media = None
        if args.media_fixtures:
            directory = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="myrica-media-fixtures-")))
            media = generate_media(directory)
        server = stack.enter_context(FixtureServer(("127.0.0.1", args.port), args.log, media))
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
