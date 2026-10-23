#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_HOME="${CARGO_HOME:-/private/tmp/ouro-cargo}"
export npm_config_cache="${npm_config_cache:-/private/tmp/ouro-tools/npm-cache}"
npm ci --prefix spec --ignore-scripts
exec python3 spec/check.py "$@"
