#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

echo "== rust"
cargo test --manifest-path core/Cargo.toml --offline

echo "== python"
cd bindings
python3 -c "import polycore; print(polycore.tokenize_words('a b c'))"
