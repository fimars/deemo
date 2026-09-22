#!/usr/bin/env python3
"""The tmux e2e workload (scripts/e2e-tmux.sh): a small HTTP server.

Serves ``$DOCROOT/index.html`` on ``127.0.0.1:$PORT`` and logs every request
to **stderr** — that log is what proves deemo captures both file descriptors
without ``2>&1``.

Deliberately not ``python3 -m http.server``: its ``HTTPServer.server_bind``
calls ``socket.getfqdn()`` (a reverse-DNS lookup) between ``bind()`` and
``listen()``, and on CI runners that lookup can stall for a minute — the test
would then measure the runner's DNS instead of deemo. Here ``listen()``
follows ``bind()`` directly.
"""

import sys
from http.server import BaseHTTPRequestHandler
from socketserver import TCPServer


class Handler(BaseHTTPRequestHandler):
    docroot = ""

    def do_GET(self):
        if self.path not in ("/", "/index.html"):
            self.send_error(404)
            return
        try:
            with open(f"{self.docroot}/index.html", "rb") as f:
                body = f.read()
        except OSError as e:
            self.send_error(500, explain=str(e))
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Server(TCPServer):
    allow_reuse_address = True  # rebind the port after an earlier round stopped


def main():
    Handler.docroot, port = sys.argv[1], int(sys.argv[2])
    with Server(("127.0.0.1", port), Handler) as server:
        server.serve_forever()


if __name__ == "__main__":
    main()
