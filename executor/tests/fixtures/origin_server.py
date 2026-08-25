#!/usr/bin/env python3
import http.server
import os
import ssl
import subprocess

MODE = os.environ.get("FIXTURE_MODE", "public")
EVIDENCE = "/evidence/trap.log"


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if MODE == "trap":
            with open(EVIDENCE, "a", encoding="utf-8") as evidence:
                evidence.write(f"{self.client_address[0]} {self.path}\n")
        if self.path == "/redirect-private":
            self.send_response(302)
            self.send_header("Location", "https://private.fixture.net/")
            self.end_headers()
            return
        body = b"fixture-ok\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        return


subprocess.run(
    [
        "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
        "-keyout", "/tmp/key.pem", "-out", "/tmp/cert.pem", "-days", "1",
        "-subj", "/CN=fixture.test",
    ],
    check=True,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
server = http.server.ThreadingHTTPServer(("0.0.0.0", 443), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain("/tmp/cert.pem", "/tmp/key.pem")
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
