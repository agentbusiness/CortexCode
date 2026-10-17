"""Decode LiveCodeBench code_generation_lite rows into task records (prepare time only).

Private test cases are stored as base64(zlib(pickle(json-string))). The official loader
(lcb_runner/benchmarks/code_generation.py) calls pickle.loads on them; here a restricted unpickler
that refuses every global is used instead, so a pickle can only yield plain data (it must be a str
holding the JSON list of tests, exactly what the official loader then json.loads).

usage: python -I decode.py <rows.jsonl> <out.jsonl>
"""
import base64
import io
import json
import pickle
import sys
import zlib


class _NoGlobals(pickle.Unpickler):
    def find_class(self, module, name):  # structural safety rule: plain data only
        raise pickle.UnpicklingError(f"global {module}.{name} refused")


def private_tests(field):
    try:
        return json.loads(field)
    except Exception:
        raw = zlib.decompress(base64.b64decode(field.encode("utf-8")))
        s = _NoGlobals(io.BytesIO(raw)).load()
        if not isinstance(s, str):
            raise ValueError(f"private tests unpickled to {type(s).__name__}, expected str")
        return json.loads(s)


def main(src, dst):
    with open(src, encoding="utf-8") as fin, open(dst, "w", encoding="utf-8") as fout:
        for line in fin:
            p = json.loads(line)
            public = json.loads(p["public_test_cases"])
            private = private_tests(p["private_test_cases"])
            meta = json.loads(p["metadata"])
            tests = public + private
            # The official evaluation sample (CodeGenerationProblem.get_evaluation_sample).
            sample = {
                "inputs": [t["input"] for t in tests],
                "outputs": [t["output"] for t in tests],
                "fn_name": meta.get("func_name", None),
            }
            rec = {
                "question_title": p["question_title"], "question_content": p["question_content"],
                "platform": p["platform"], "question_id": p["question_id"], "contest_id": p["contest_id"],
                "contest_date": p["contest_date"], "starter_code": p["starter_code"], "difficulty": p["difficulty"],
                "metadata": meta, "n_public": len(public), "n_private": len(private),
                "testtypes": sorted({t["testtype"] for t in tests}), "sample": sample,
            }
            fout.write(json.dumps(rec) + "\n")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
