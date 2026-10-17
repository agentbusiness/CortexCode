"""Extract BigCodeBench-Hard rows from the pinned parquet file to JSON lines (prepare time only).

usage: python -I extract.py <file.parquet> <out.jsonl>   (needs pyarrow)
"""
import json
import sys

import pyarrow.parquet as pq

FIELDS = ["task_id", "complete_prompt", "instruct_prompt", "canonical_solution", "code_prompt", "test", "entry_point", "libs"]

with open(sys.argv[2], "w", encoding="utf-8") as out:
    for row in pq.read_table(sys.argv[1]).to_pylist():
        rec = {k: row[k] for k in FIELDS}
        if isinstance(rec["libs"], str):
            # libs is stored as the repr of a list of module names; parse it structurally.
            import ast
            rec["libs"] = ast.literal_eval(rec["libs"])
        out.write(json.dumps(rec) + "\n")
