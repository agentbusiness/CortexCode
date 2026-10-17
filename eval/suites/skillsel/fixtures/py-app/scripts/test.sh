#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
filter=$1
python3 -m pytest -q tests ${filter:+-k "$filter"}
