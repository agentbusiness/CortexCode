import json
import threading
import urllib.request
from http.server import HTTPServer

from app.main import Handler


def test_health():
    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{server.server_port}/health") as resp:
            assert json.load(resp) == {"ok": True}
    finally:
        server.shutdown()
