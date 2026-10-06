#!/usr/bin/env python3
"""Tiny static server for local development: `python serve.py [port]`.

Python's default server relies on the OS MIME registry, which on Windows
labels .js as text/plain and breaks ES modules; this pins the types we need.
"""
import sys
from pathlib import Path
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer


class Handler(SimpleHTTPRequestHandler):
    extensions_map = {
        **SimpleHTTPRequestHandler.extensions_map,
        ".js": "text/javascript",
        ".mjs": "text/javascript",
        ".wasm": "application/wasm",
        ".webmanifest": "application/manifest+json",
        ".svg": "image/svg+xml",
        ".pdf": "application/pdf",
    }

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    server = ThreadingHTTPServer(("127.0.0.1", port), partial(Handler, directory=str(Path(__file__).resolve().parent / "web")))
    print(f"Serving web/ at http://127.0.0.1:{port}")
    server.serve_forever()
