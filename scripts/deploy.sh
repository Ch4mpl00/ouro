#!/usr/bin/env bash
# Deploy from this machine: build the image here, ship it over SSH, restart.
#
#   scripts/deploy.sh              # every service
#   scripts/deploy.sh mcp agent    # only these
#
# The droplet (1 vCPU / 2 GB) is too small to compile the Rust crate, so it
# never builds: it receives a finished linux/amd64 image and runs
# `docker compose up --no-build` from a checkout of the SAME commit the image
# was built from, so compose config and image never drift apart.
#
# Env: DEPLOY_HOST (root@165.22.199.65), DEPLOY_DIR (/root/agent-helper),
#      DEPLOY_PLATFORM (linux/amd64).

set -euo pipefail

HOST="${DEPLOY_HOST:-root@165.22.199.65}"
DIR="${DEPLOY_DIR:-/root/agent-helper}"
PLATFORM="${DEPLOY_PLATFORM:-linux/amd64}"
IMAGE="mcp-tools-image:latest"

cd "$(git rev-parse --show-toplevel)"
sha="$(git rev-parse HEAD)"

# The image is built from the working tree, the droplet checks out the
# commit: both must be the same thing, and the commit must be fetchable.
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "deploy: uncommitted changes — commit (and push) first" >&2
  exit 1
fi
git fetch -q origin
if [ -z "$(git branch -r --contains "$sha")" ]; then
  echo "deploy: $sha is not on origin — push first" >&2
  exit 1
fi

echo "deploy: building $IMAGE for $PLATFORM at ${sha:0:12}"
docker buildx build --platform "$PLATFORM" --tag "$IMAGE" --load .

echo "deploy: shipping image to $HOST"
docker save "$IMAGE" | gzip -1 | ssh "$HOST" 'gunzip | docker load'

echo "deploy: restarting ${*:-all services} on $HOST"
# shellcheck disable=SC2029 # $DIR, $sha and the service list expand here on purpose
ssh "$HOST" "set -e
  cd '$DIR'
  if [ -n \"\$(git status --porcelain --untracked-files=no)\" ]; then
    echo 'deploy: the droplet checkout has local changes — refusing to switch commits' >&2
    exit 1
  fi
  git fetch -q origin
  git checkout -q --detach '$sha'
  docker compose up -d --no-build $*
  docker image prune -f >/dev/null
  docker compose ps --format 'table {{.Service}}\t{{.Status}}'"
