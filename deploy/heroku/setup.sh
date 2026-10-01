#!/usr/bin/env bash
# =============================================================================
# Scrapix Heroku Setup Script
# =============================================================================
# Provisions one Heroku app:
#   scrapix-api — Rust API (scrapix all mode, hosted) + Postgres addon (the
#                 engine's own job store)
# The console and the Rails control plane (the Lab) are deployed from
# meilisearch/lab.
#
# Prerequisites:
#   - Heroku CLI installed and logged in (`heroku login`)
#   - A Meilisearch Cloud instance (or self-hosted)
#   - (Optional) ClickHouse Cloud instance
#   - (Optional) Anthropic API key for AI enrichment
#
# Usage:
#   1. Edit the variables below
#   2. Run: bash deploy/heroku/setup.sh
# =============================================================================

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration — edit these before running
# ---------------------------------------------------------------------------
API_APP_NAME="scrapix-api"
HEROKU_TEAM="meili"

# Meilisearch Cloud
MEILISEARCH_URL=""        # e.g. https://ms-xxxx.meilisearch.io
MEILISEARCH_API_KEY=""    # master key

# ClickHouse (optional — leave empty to disable analytics)
CLICKHOUSE_URL=""         # e.g. https://ka2htxje0a.eu-central-1.aws.clickhouse.cloud:8443
CLICKHOUSE_DATABASE="scrapix_prod"
CLICKHOUSE_USER="default"
CLICKHOUSE_PASSWORD=""

# Lab (Rails control plane): the hosted engine reports usage events there,
# calls its /internal/* API, and refuses to start without LAB_URL. Rails needs
# the SAME LAB_EVENTS_SECRET / LAB_SERVICE_TOKEN (printed at the end of this
# script).
LAB_URL=""                # e.g. https://lab.example.com (the base URL, no path)

# AI enrichment (optional)
AI_PROVIDER="anthropic"
ANTHROPIC_API_KEY=""

# CORS — add the console's origin (and any other) here, comma-separated;
# *.meilisearch.com is always allowed
CORS_ORIGINS=""

# ---------------------------------------------------------------------------
# Validate configuration — before any `heroku` call, so a missing value never
# leaves a half-created app or a paid add-on behind
# ---------------------------------------------------------------------------
case "${LAB_URL}" in
    http://*|https://*) ;;
    *)
        echo "LAB_URL must be set to the Lab's http(s) base URL" >&2
        echo "(the hosted engine refuses to start without it). Edit the config block above." >&2
        exit 1
        ;;
esac

LAB_EVENTS_SECRET=$(openssl rand -hex 32)
LAB_SERVICE_TOKEN=$(openssl rand -hex 32)

# ---------------------------------------------------------------------------
# Create API app
# ---------------------------------------------------------------------------
echo "==> Creating API app: ${API_APP_NAME}"
heroku create "${API_APP_NAME}" --team "${HEROKU_TEAM}" --region eu --stack container

echo "==> Adding Postgres addon"
heroku addons:create heroku-postgresql:essential-0 -a "${API_APP_NAME}"

echo "==> Setting API config vars"

# Build config vars, skipping empty optional ones
CONFIG_VARS=(
    "SCRAPIX_MODE=hosted"
    "LAB_URL=${LAB_URL}"
    "LAB_EVENTS_SECRET=${LAB_EVENTS_SECRET}"
    "LAB_SERVICE_TOKEN=${LAB_SERVICE_TOKEN}"
    "RUST_LOG=info"
)

[ -n "${MEILISEARCH_URL}" ]     && CONFIG_VARS+=("MEILISEARCH_URL=${MEILISEARCH_URL}")
[ -n "${MEILISEARCH_API_KEY}" ] && CONFIG_VARS+=("MEILISEARCH_API_KEY=${MEILISEARCH_API_KEY}")
[ -n "${CLICKHOUSE_URL}" ]      && CONFIG_VARS+=("CLICKHOUSE_URL=${CLICKHOUSE_URL}")
[ -n "${CLICKHOUSE_DATABASE}" ] && CONFIG_VARS+=("CLICKHOUSE_DATABASE=${CLICKHOUSE_DATABASE}")
[ -n "${CLICKHOUSE_USER}" ]     && CONFIG_VARS+=("CLICKHOUSE_USER=${CLICKHOUSE_USER}")
[ -n "${CLICKHOUSE_PASSWORD}" ] && CONFIG_VARS+=("CLICKHOUSE_PASSWORD=${CLICKHOUSE_PASSWORD}")
[ -n "${AI_PROVIDER}" ]         && CONFIG_VARS+=("AI_PROVIDER=${AI_PROVIDER}")
[ -n "${ANTHROPIC_API_KEY}" ]   && CONFIG_VARS+=("ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY}")
[ -n "${CORS_ORIGINS}" ]        && CONFIG_VARS+=("CORS_ORIGINS=${CORS_ORIGINS}")

heroku config:set -a "${API_APP_NAME}" "${CONFIG_VARS[@]}"

API_URL="https://${API_APP_NAME}.herokuapp.com"

# ---------------------------------------------------------------------------
# Deploy instructions
# ---------------------------------------------------------------------------
echo ""
echo "============================================"
echo " Setup complete!"
echo "============================================"
echo ""
echo "Next steps:"
echo ""
echo "  1. Deploy the API (from repo root):"
echo "     git remote add heroku-api https://git.heroku.com/${API_APP_NAME}.git"
echo "     git push heroku-api main"
echo ""
echo "  2. Check logs:"
echo "     heroku logs -a ${API_APP_NAME} --tail"
echo ""
echo "  Set the SAME LAB_EVENTS_SECRET and LAB_SERVICE_TOKEN on the Lab"
echo "  (deployed from meilisearch/lab):"
echo "     heroku config -a ${API_APP_NAME} | grep LAB_"
echo ""
echo "  API URL: ${API_URL}"
echo ""
