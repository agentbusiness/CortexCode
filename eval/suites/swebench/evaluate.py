"""Grade one SWE-bench prediction with the official harness (swebench 4.1.0, library calls).

This is `python -m swebench.harness.run_evaluation` for a single instance, done through the same
functions the command calls (make_test_spec, build_env_images, run_instance), because the command
always builds x86_64 test specs: `make_test_spec`'s `arch` parameter is not reachable from its CLI.
Passing `arch` here selects the harness's own native arm64 images (sweb.*.arm64.*) on Apple Silicon.

usage:
  python -I evaluate.py --instance instance.json (--patch model.patch | --gold) --model-name NAME
      --run-id ID --namespace swebench|none --arch arm64|x86_64 --log-root DIR [--timeout 1800]

The harness writes its logs under <log-root>/logs/run_evaluation/<run_id>/<model>/<instance_id>/
(run_instance.log, patch.diff, eval.sh, test_output.txt, report.json), exactly as the command does
from its working directory. The last stdout line is a JSON summary.
"""

import argparse
import json
import os
import sys
import traceback
from pathlib import Path


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--instance", required=True, help="dataset row as JSON (SWE-bench Verified)")
    ap.add_argument("--patch", default=None, help="model patch file")
    ap.add_argument("--gold", action="store_true", help="grade the reference patch (control)")
    ap.add_argument("--model-name", required=True)
    ap.add_argument("--run-id", required=True)
    ap.add_argument("--namespace", default="swebench", help='image namespace; "none" = images built locally')
    ap.add_argument("--arch", choices=["x86_64", "arm64"], required=True)
    ap.add_argument("--timeout", type=int, default=1800, help="run_evaluation's per-instance test timeout")
    ap.add_argument("--log-root", required=True)
    args = ap.parse_args()

    instance = json.loads(Path(args.instance).read_text())
    instance_id = instance["instance_id"]
    patch = instance["patch"] if args.gold else Path(args.patch).read_text()
    namespace = None if args.namespace.strip().lower() in ("", "none", "null") else args.namespace

    os.makedirs(args.log_root, exist_ok=True)
    os.chdir(args.log_root)

    summary = {
        "instance_id": instance_id, "run_id": args.run_id, "model_name_or_path": args.model_name,
        "namespace": namespace, "arch": args.arch, "gold": args.gold, "patch_bytes": len(patch.encode()),
    }
    # run_evaluation drops predictions whose patch is empty before any container starts
    # (get_dataset_from_preds: empty_patch_ids) and the run report counts them as unresolved.
    if not patch.strip():
        summary.update(status="empty_patch", completed=False, resolved=False)
        print(json.dumps(summary))
        return 0

    import docker
    import swebench
    from swebench.harness.docker_build import build_env_images
    from swebench.harness.run_evaluation import run_instance
    from swebench.harness.test_spec.test_spec import make_test_spec

    summary["swebench_version"] = swebench.__version__
    spec = make_test_spec(instance, namespace=namespace, arch=args.arch)
    summary["image"] = spec.instance_image_key
    client = docker.from_env()
    try:
        if namespace is None:
            # Local images: run_evaluation builds the base and environment images first
            # (build_env_images), then run_instance builds the instance image on top.
            build_env_images(client, [spec], False, 1)
        pred = {"instance_id": instance_id, "model_name_or_path": args.model_name, "model_patch": patch}
        res = run_instance(spec, pred, False, False, client, args.run_id, args.timeout)
        summary.update(completed=bool(res.get("completed")), resolved=bool(res.get("resolved")))
    except Exception as e:  # recorded; the instance counts as unresolved, as in run_evaluation
        summary.update(completed=False, resolved=False, error=f"{type(e).__name__}: {e}", traceback=traceback.format_exc()[-4000:])
    log_dir = Path("logs/run_evaluation") / args.run_id / args.model_name.replace("/", "__") / instance_id
    summary["log_dir"] = str(log_dir.resolve())
    summary["status"] = "evaluated" if summary.get("completed") else "not_completed"
    print(json.dumps(summary))
    return 0


if __name__ == "__main__":
    sys.exit(main())
