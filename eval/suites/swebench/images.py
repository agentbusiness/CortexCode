"""Make one SWE-bench instance image available locally, the way the official harness does.

usage:
  python -I images.py --instance instance.json --source hub-arm64|hub-x86_64|local-arm64 --log-root DIR

  hub-x86_64   swebench/sweb.eval.x86_64.<id>:latest, pulled (the published images every leaderboard
               run uses; on Apple Silicon Docker runs them under emulation)
  hub-arm64    swebench/sweb.eval.arm64.<id>:latest, pulled (published for 281 of the 500 Verified
               instances; see images.lock.json)
  local-arm64  sweb.eval.arm64.<id>:latest built natively by the harness itself
               (build_env_images + build_instance_image with arch="arm64", namespace=None); the
               harness's Dockerfiles install the aarch64 Miniconda for arm64

The harness names the image (TestSpec.instance_image_key) and pulls or builds it exactly as
run_instance would. Build logs go under <log-root>/logs/build_images/. The last stdout line is JSON:
{"image", "namespace", "arch", "ok", "error", "repo_digests", "architecture"}.
"""

import argparse
import json
import os
import sys
import traceback
from pathlib import Path

SOURCES = {
    "hub-x86_64": ("swebench", "x86_64"),
    "hub-arm64": ("swebench", "arm64"),
    "local-arm64": (None, "arm64"),
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--instance", required=True)
    ap.add_argument("--source", choices=sorted(SOURCES), required=True)
    ap.add_argument("--log-root", required=True)
    args = ap.parse_args()

    import docker
    from swebench.harness.docker_build import build_env_images, build_instance_image
    from swebench.harness.test_spec.test_spec import make_test_spec

    instance = json.loads(Path(args.instance).read_text())
    namespace, arch = SOURCES[args.source]
    spec = make_test_spec(instance, namespace=namespace, arch=arch)
    out = {"instance_id": instance["instance_id"], "source": args.source, "image": spec.instance_image_key,
           "namespace": namespace, "arch": arch, "platform": spec.platform, "ok": False, "error": None}
    os.makedirs(args.log_root, exist_ok=True)
    os.chdir(args.log_root)
    client = docker.from_env(timeout=1800)
    try:
        try:
            client.images.get(spec.instance_image_key)
        except docker.errors.ImageNotFound:
            if spec.is_remote_image:
                client.images.pull(spec.instance_image_key)
            else:
                build_env_images(client, [spec], False, 1)
                build_instance_image(spec, client, None, False)
        img = client.images.get(spec.instance_image_key)
        out.update(ok=True, repo_digests=img.attrs.get("RepoDigests", []), architecture=img.attrs.get("Architecture"),
                   image_id=img.id)
    except Exception as e:
        out.update(error=f"{type(e).__name__}: {e}", traceback=traceback.format_exc()[-4000:])
    print(json.dumps(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
