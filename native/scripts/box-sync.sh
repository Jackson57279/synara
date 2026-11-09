#!/usr/bin/env bash
# Push the current branch into an instabox sandbox checkout at ~/synara.
#
# Usage: native/scripts/box-sync.sh <box> [branch]
# Sends only commits the box does not have yet when it already has a checkout;
# the first run sends a full bundle. Uncommitted changes are not sent, and
# local edits inside the box's checkout are discarded (history rewrites are fine).
set -euo pipefail

box="${1:?usage: box-sync.sh <box> [branch]}"
branch="${2:-$(git rev-parse --abbrev-ref HEAD)}"
repo_root="$(git rev-parse --show-toplevel)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

remote_head="$(instabox ssh "$box" -- "git -C ~/synara rev-parse --verify -q HEAD 2>/dev/null || true" 2>/dev/null | tail -1 | tr -d '[:space:]')"

if [[ -n "$remote_head" ]] && git -C "$repo_root" cat-file -e "${remote_head}^{commit}" 2>/dev/null; then
  range="${remote_head}..${branch}"
  if [[ -z "$(git -C "$repo_root" rev-list "$range")" ]]; then
    echo "box already at $(git -C "$repo_root" rev-parse --short "$branch")"
    exit 0
  fi
  git -C "$repo_root" bundle create "$work/sync.bundle" "$range" >/dev/null 2>&1
else
  git -C "$repo_root" bundle create "$work/sync.bundle" "$branch" >/dev/null 2>&1
fi

instabox cp "$work/sync.bundle" "$box:sync.bundle" >/dev/null
instabox ssh "$box" -- "set -e
  if [ ! -d ~/synara/.git ]; then git clone -q -b '$branch' ~/sync.bundle ~/synara; fi
  cd ~/synara
  git fetch -q -f ~/sync.bundle '+$branch:refs/remotes/sync/$branch'
  git checkout -q -f '$branch' 2>/dev/null || git checkout -q -f -b '$branch' 'sync/$branch'
  git reset -q --hard 'sync/$branch'
  git log --oneline -1"
