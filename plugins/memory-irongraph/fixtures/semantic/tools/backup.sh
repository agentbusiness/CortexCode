#!/usr/bin/env bash
# Archive a directory into a dated tarball and keep only the newest seven.
make_snapshot() {
  local source="$1"
  tar -czf "backup-$(date +%Y%m%d).tar.gz" "$source"
  ls -t backup-*.tar.gz | tail -n +8 | xargs rm -f
}

# Print free disk space of the current volume in gigabytes.
disk_free() {
  df -g . | awk 'NR==2 {print $4}'
}
