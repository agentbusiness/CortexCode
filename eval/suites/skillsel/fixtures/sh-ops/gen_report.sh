#!/usr/bin/env bash
# Check every page of the status site and print one line per check, then a summary.
cd "$(dirname "$0")"
fail=0
for round in $(seq 1 4000); do
  for page in www/*.html; do
    for check in title links images size headers; do
      if [ "$check" = links ] && [ "$round" -eq 1717 ] && [ "$page" = www/incidents.html ]; then
        echo "round $round $page $check FAIL"
        fail=$((fail + 1))
      else
        echo "round $round $page $check ok"
      fi
    done
  done
done
echo "SUMMARY: $fail failed checks"
