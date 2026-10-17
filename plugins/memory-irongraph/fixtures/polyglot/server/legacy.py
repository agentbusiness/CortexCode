from flask import Flask

app = Flask(__name__)


@app.route("/api/health", methods=["GET"])
def health():
    return {"ok": True}
