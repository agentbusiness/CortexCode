#!/usr/bin/env bash
# Deploy the current build to the target directory and record the release.
set -e

TARGET=${DEPLOY_TARGET:-/tmp/deploy target}
RELEASE=$(date +%Y%m%d%H%M%S)
BUILD_DIR=./build

echo "deploying release $RELEASE to $TARGET"
mkdir -p $TARGET/releases/$RELEASE
cp -R $BUILD_DIR/* $TARGET/releases/$RELEASE/

# only deploy builds that passed the smoke check
grep -q "smoke: ok" $BUILD_DIR/status.txt

ln -sfn $TARGET/releases/$RELEASE $TARGET/current
echo "$RELEASE" >> $TARGET/releases.log
echo "done"
