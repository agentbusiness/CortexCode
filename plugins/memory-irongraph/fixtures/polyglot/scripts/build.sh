#!/usr/bin/env bash
set -e
source ./scripts/lib.sh

build_web() {
  log_step "web" "$BUILD_MODE"
  echo "$API_URL"
}

build_web
