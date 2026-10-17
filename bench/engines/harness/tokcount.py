"""Token counter service: one JSON request per stdin line, one JSON reply per stdout line.

Request {"id": any, "texts": [str, ...]} -> reply {"id": any, "counts": [int, ...], "ids": [[int,...],...]?}
Request {"id": any, "texts": [...], "with_ids": true} also returns the token ids.
Uses Qwen3.8's own tokenizer.json (HF `tokenizers`), so every engine's output is counted the same way.
Usage: uv run --with tokenizers python -I tokcount.py <tokenizer.json>
"""

import json
import sys

from tokenizers import Tokenizer

tok = Tokenizer.from_file(sys.argv[1])
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    encs = tok.encode_batch(req["texts"], add_special_tokens=False)
    rep = {"id": req.get("id"), "counts": [len(e.ids) for e in encs]}
    if req.get("with_ids"):
        rep["ids"] = [e.ids for e in encs]
    sys.stdout.write(json.dumps(rep) + "\n")
    sys.stdout.flush()
