"""Extract SWE-bench Verified rows from the pinned parquet file to JSON lines (prepare time only).

usage: python -I extract.py <file.parquet> <out.jsonl>   (needs pyarrow)

Rows are written unchanged (every dataset column, FAIL_TO_PASS / PASS_TO_PASS stay JSON strings as
published), because the harness's make_test_spec reads the row itself.
"""
import json
import sys

import pyarrow.parquet as pq

with open(sys.argv[2], "w", encoding="utf-8") as out:
    for row in pq.read_table(sys.argv[1]).to_pylist():
        out.write(json.dumps(row) + "\n")
