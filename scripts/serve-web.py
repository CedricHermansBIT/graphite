#!/usr/bin/env python3
"""Development server with the headers required by shared Wasm memory."""
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

class Handler(SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header('Cross-Origin-Opener-Policy', 'same-origin')
        self.send_header('Cross-Origin-Embedder-Policy', 'require-corp')
        super().end_headers()

if __name__ == '__main__':
    root = Path(__file__).resolve().parent.parent / 'web'
    import os
    os.chdir(root)
    print('Serving Graphite Web at http://127.0.0.1:8080')
    ThreadingHTTPServer(('127.0.0.1', 8080), Handler).serve_forever()
