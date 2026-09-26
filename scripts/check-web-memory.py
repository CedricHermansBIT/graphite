#!/usr/bin/env python3
"""Fail the web build if wasm-bindgen generated unshared memory."""
from pathlib import Path
import re
import sys

source = Path(sys.argv[1]).read_text()
if not re.search(r"new WebAssembly\.Memory\(\{[^}]*\bshared\s*:\s*true\b", source):
    sys.exit("Web build has no shared WebAssembly memory; Rayon workers cannot start")
print("Verified shared WebAssembly memory for Rayon workers")
