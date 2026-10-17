#!/usr/bin/env bash
# Interactive setup: asks a few questions and writes settings.env.
set -euo pipefail

read -r -p "Install directory [/opt/statuspage]: " dir
read -r -p "HTTP port [8080]: " port
read -r -p "Enable metrics? [y/N]: " metrics
read -r -s -p "Admin password (empty: generate one): " password
echo

dir=${dir:-/opt/statuspage}
port=${port:-8080}
metrics=${metrics:-n}
password=${password:-$(head -c 12 /dev/urandom | base64)}

cat > settings.env <<EOF
INSTALL_DIR=$dir
PORT=$port
METRICS=$metrics
ADMIN_PASSWORD=$password
EOF
echo "wrote settings.env"
