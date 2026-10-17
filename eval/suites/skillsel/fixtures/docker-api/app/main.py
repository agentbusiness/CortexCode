"""Tiny JSON API: GET /health and GET /items."""
import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer

ITEMS = [{"id": 1, "name": "lamp"}, {"id": 2, "name": "desk"}]


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            body = {"ok": True}
        elif self.path == "/items":
            body = ITEMS
        else:
            self.send_error(404)
            return
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    port = int(os.environ.get("PORT", "8000"))
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
