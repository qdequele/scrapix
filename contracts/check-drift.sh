#!/usr/bin/env bash
# Fails when a vendored contract differs from its owner's copy on main.
# Owner copies come from $LAB_SRC (a local checkout of meilisearch/lab) when
# set, else from GitHub via `gh api` (meilisearch/lab is private, so this needs
# GH_TOKEN or LAB_REPO_TOKEN). With neither, the check is skipped (exit 0).
# `--fix` rewrites the vendored copies.
set -euo pipefail
cd "$(dirname "$0")"
FILES=(lab-internal.openapi.json lab-events.schema.json)
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
# The one owner file the Lab has not published yet: it alone may be absent
# (a missing file or an HTTP 404), and only that is skipped. Any other
# failure, for any file, is fatal.
NOT_YET_PUBLISHED=lab-events.schema.json
owner="$(mktemp)"
err="$(mktemp)"
trap 'rm -f "$owner" "$err"' EXIT
status=0
for f in "${FILES[@]}"; do
  vendored="vendor/lab/$f"
  if [ -n "${LAB_SRC:-}" ]; then
    if [ ! -f "$LAB_SRC/contracts/$f" ] && [ "$f" = "$NOT_YET_PUBLISHED" ] && [ -d "$LAB_SRC/contracts" ]; then
      echo "notice: $LAB_SRC/contracts/$f does not exist yet on the Lab; skipping $vendored"
      continue
    fi
    cp "$LAB_SRC/contracts/$f" "$owner"
  else
    if ! gh api "repos/meilisearch/lab/contents/contracts/$f" -H 'Accept: application/vnd.github.raw' > "$owner" 2> "$err"; then
      if [ "$f" = "$NOT_YET_PUBLISHED" ] && grep -qE '404|Not Found' "$err"; then
        echo "notice: meilisearch/lab main has no contracts/$f yet; skipping $vendored"
        continue
      fi
      cat "$err" >&2
      echo "error: cannot fetch meilisearch/lab main:contracts/$f" >&2
      exit 1
    fi
  fi
  if ! cmp -s "$owner" "$vendored"; then
    if $fix; then cp "$owner" "$vendored"; echo "updated $vendored"
    else echo "DRIFT: $vendored differs from meilisearch/lab main:contracts/$f"; diff -u "$vendored" "$owner" | head -50 || true; status=1; fi
  else
    echo "ok: $vendored"
  fi
done
exit $status
