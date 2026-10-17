#!/bin/bash
if [ "$(cat /app/out/hello.txt)" = "hello" ] && [ -s /root/jq-version.txt ]; then
  echo 1 > /logs/verifier/reward.txt
else
  echo 0 > /logs/verifier/reward.txt
fi
