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
# calls its /internal/* API, and refuses to start without LAB_URL,
# LAB_INSTANCE_ID, LAB_INSTANCE_SECRET and LAB_SERVICE_TOKEN. Order:
#   1. LAB_SERVICE_TOKEN: generate it first (`openssl rand -hex 32`; run this
#      script with it empty and it prints one plus the mint command);
#   2. on the Lab, mint this engine's credentials with that token:
#      bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=eu \
#        URL=https://<API_APP_NAME>.herokuapp.com CREDENTIAL=<LAB_SERVICE_TOKEN>
#      It prints LAB_URL, LAB_INSTANCE_ID and LAB_INSTANCE_SECRET once;
#   3. fill in the four values below and run this script.
LAB_SERVICE_TOKEN=""      # >= 32 chars; passed to the Lab at mint time as CREDENTIAL=
LAB_URL=""                # LAB_URL printed by the Lab (the base URL, no path)
LAB_INSTANCE_ID=""        # LAB_INSTANCE_ID printed by the Lab (uuid)
LAB_INSTANCE_SECRET=""    # LAB_INSTANCE_SECRET printed by the Lab (64 hex)

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
if [ -z "${LAB_SERVICE_TOKEN}" ]; then
    token=$(openssl rand -hex 32)
    echo "LAB_SERVICE_TOKEN is not set. Generated one for you:" >&2
    echo "  LAB_SERVICE_TOKEN=\"${token}\"" >&2
    echo "Put it in the config block above, then mint this engine's credentials on the Lab:" >&2
    echo "  bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=eu \\" >&2
    echo "    URL=https://${API_APP_NAME}.herokuapp.com CREDENTIAL=${token}" >&2
    echo "and fill in the LAB_URL, LAB_INSTANCE_ID and LAB_INSTANCE_SECRET it prints." >&2
    exit 1
fi
if [ "${#LAB_SERVICE_TOKEN}" -lt 32 ]; then
    echo "LAB_SERVICE_TOKEN must be at least 32 characters (openssl rand -hex 32)" >&2
    exit 1
fi

case "${LAB_URL}" in
    http://*|https://*) ;;
    *)
        echo "LAB_URL must be set to the Lab's http(s) base URL" >&2
        echo "(the hosted engine refuses to start without it). Edit the config block above." >&2
        exit 1
        ;;
esac

if [ -z "${LAB_INSTANCE_ID}" ] || [ -z "${LAB_INSTANCE_SECRET}" ]; then
    echo "LAB_INSTANCE_ID and LAB_INSTANCE_SECRET must be set. Mint them on the Lab with" >&2
    echo "this engine's LAB_SERVICE_TOKEN:" >&2
    echo "  bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=eu \\" >&2
    echo "    URL=https://${API_APP_NAME}.herokuapp.com CREDENTIAL=<LAB_SERVICE_TOKEN>" >&2
    echo "then edit the config block above." >&2
    exit 1
fi

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
    "LAB_INSTANCE_ID=${LAB_INSTANCE_ID}"
    "LAB_INSTANCE_SECRET=${LAB_INSTANCE_SECRET}"
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
echo "  API URL: ${API_URL}"
echo ""
