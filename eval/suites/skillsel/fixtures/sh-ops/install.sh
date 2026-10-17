#!/bin/sh
# Install the ops scripts into $PREFIX/bin.
PREFIX=${PREFIX:-/usr/local}
scripts=(backup.sh gen_report.sh)

if [[ ! -d "$PREFIX/bin" ]]; then
  mkdir -p "$PREFIX/bin"
fi

for s in "${scripts[@]}"; do
  install -m 755 "$s" "$PREFIX/bin/${s%.sh}"
done
echo "installed ${#scripts[@]} scripts to $PREFIX/bin"
