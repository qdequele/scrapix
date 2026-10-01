#!/usr/bin/env bash
# Fails when a vendored contract differs from its owner's copy on main.
# Owner copies come from $LAB_SRC (a local checkout of meilisearch/lab) when
# set, else from GitHub via `gh api` (meilisearch/lab is private, so this needs
# GH_TOKEN or LAB_REPO_TOKEN). With neither, the check is skipped (exit 0).
# `--fix` rewrites the vendored copies.
set -euo pipefail
cd "$(dirname "$0")"
FILES=(lab-internal.openapi.json)
fix=false
case "${1:-}" in
  "") ;;
  --fix) fix=true ;;
  *) echo "usage: $0 [--fix]" >&2; exit 2 ;;
esac
[ "$#" -le 1 ] || { echo "usage: $0 [--fix]" >&2; exit 2; }
if [ -z "${LAB_SRC:-}" ]; then
  if [ -z "${GH_TOKEN:-}" ] && [ -n "${LAB_REPO_TOKEN:-}" ]; then
    export GH_TOKEN="$LAB_REPO_TOKEN"
  fi
  if [ -z "${GH_TOKEN:-}" ]; then
    echo "notice: LAB_REPO_TOKEN not set; skipping the lab contract drift check"
    exit 0
  fi
fi
owner="$(mktemp)"
trap 'rm -f "$owner"' EXIT
status=0
for f in "${FILES[@]}"; do
  vendored="vendor/lab/$f"
  if [ -n "${LAB_SRC:-}" ]; then
    cp "$LAB_SRC/contracts/$f" "$owner"
  else
    gh api "repos/meilisearch/lab/contents/contracts/$f" -H 'Accept: application/vnd.github.raw' > "$owner"
  fi
  if ! cmp -s "$owner" "$vendored"; then
    if $fix; then cp "$owner" "$vendored"; echo "updated $vendored"
    else echo "DRIFT: $vendored differs from meilisearch/lab main:contracts/$f"; diff -u "$vendored" "$owner" | head -50 || true; status=1; fi
  else
    echo "ok: $vendored"
  fi
done
exit $status
