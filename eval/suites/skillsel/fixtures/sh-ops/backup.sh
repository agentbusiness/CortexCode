#!/usr/bin/env bash
# Copy the site and its settings to backups/<date>/.
set -euo pipefail
cd "$(dirname "$0")"
dest="backups/$(date +%F)"
mkdir -p "$dest"
cp -R www "$dest/"
[ -f settings.env ] && cp settings.env "$dest/"
echo "backup written to $dest"
