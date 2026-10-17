#!/bin/sh
# Runs a uzu CLI with the M model's speculator disabled: uzu enables the speculator only when
# speculator/shapes.json has an entry for this GPU (or "default") (dflash_tfm.rs:104-112), so the
# cache copy's shapes.json is replaced by one without entries for the run and restored afterwards.
# The verified download in ~/.cortexcode/models is a hardlink and is never written: the cache entry
# is unlinked and a new file is written in its place.
C="$HOME/.cache/mirai/models/mirai/alibaba-qwen3.8-27b-mirai-mirai-m-4/8b878f57094a616c0ea4c214fe774af0cbfb4353/speculator"
ORIG="$HOME/.cortexcode/models/trymirai-Qwen3.8-27B-M/8b878f57094a616c0ea4c214fe774af0cbfb4353/speculator/shapes.json"
restore() { rm -f "$C/shapes.json"; ln "$ORIG" "$C/shapes.json"; }
trap 'restore; exit 143' TERM INT
rm -f "$C/shapes.json"; printf '{}\n' > "$C/shapes.json"
"$@" &
child=$!
trap 'kill -TERM $child 2>/dev/null; wait $child; restore; exit 143' TERM INT
wait $child; rc=$?
restore
exit $rc
