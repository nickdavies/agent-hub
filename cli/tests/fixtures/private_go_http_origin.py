#!/usr/bin/env python3
"""Deterministic authenticated dumb-HTTP Git origin for attended Docker tests."""

import base64
import http.server
import os
from pathlib import Path


ROOT = Path(os.environ["FIXTURE_GIT_ROOT"]).resolve()
EVIDENCE = Path(os.environ["FIXTURE_EVIDENCE"]).resolve()
TOKEN = Path(os.environ["FIXTURE_TOKEN_FILE"]).read_text(encoding="utf-8").strip()
EXPECTED = "Basic " + base64.b64encode(("x-access-token:" + TOKEN).encode()).decode()


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(ROOT), **kwargs)

    def do_GET(self):
        if self.headers.get("Authorization") != EXPECTED:
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="private-go-fixture"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        with EVIDENCE.open("a", encoding="utf-8") as stream:
            # Never persist Authorization or the synthetic token.
            stream.write("authorized GET " + self.path.split("?", 1)[0] + "\n")
        super().do_GET()

    def log_message(self, *_args):
        pass


http.server.ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
