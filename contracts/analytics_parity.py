#!/usr/bin/env python3
"""Diff the analytics pipes between the Rails app and the Rust engine.

Lab split phase 1: the engine now serves `/analytics/v0/pipes/*` scoped per
account, and Rails is the authoritative copy until it is deleted. This script
calls every pipe on both backends with the same API key and compares the parsed
JSON bodies (and status codes), ignoring only `statistics.elapsed`. Run it with
both backends up and reading the same ClickHouse, after seeding it with a
scrape and a small crawl:

    python3 contracts/analytics_parity.py \\
        --rails http://localhost:8081 --engine http://localhost:8080 \\
        --api-key "$KEY"

Options:
    --account-id ID   the key's account (default: looked up, see below)
    --job-id ID       a real job of that account for the job_* pipes
                      (default: job_test, which has no data)
    --domain NAME     a real crawled domain for domain_stats (default:
                      example.com)

The account id comes from `GET {rails}/account` with the key (that route is
session-only today, so it usually refuses an API key), then from the
`account_id` column of Rails' own `account_usage` pipe, which always returns the
caller's account. Every `account_id=` in the path list is that id; a path that
named a different account is dropped (both backends answer 404 for it).

Known, accepted differences (documented, not fixed; fix the engine only when a
difference is NOT one of these, since Rails is authoritative):

- kpis `avg_duration_ms`: Rails sums `avg_duration_ms * total_requests` and
  divides in Ruby; when the ClickHouse values are integral Ruby integer-divides
  and the engine divides as floats. Expect a small difference there on data
  with integral durations.
- Integer parameters: Rails uses `to_i` (so `hours=abc` becomes 0), the engine
  falls back to the default (24 / 30 / 20). This script only sends valid
  integers, so it does not exercise the difference.
- Timezone: Rails reads ClickHouse times as UTC and labels them `Z`; against a
  ClickHouse server whose timezone is not UTC the two sides can disagree on the
  hourly/daily/timeline timestamps. Run the parity check against a UTC server.

Numbers are compared as parsed JSON, so 12 == 12.0 is not a difference.
"""

import argparse
import difflib
import json
import sys
import urllib.error
import urllib.request
from urllib.parse import quote

ACCOUNT = "{account}"


def paths(domain: str, job_id: str) -> list[str]:
    job = quote(job_id)
    return [
        "pipes",
        "pipes?account_id=" + ACCOUNT,
        "pipes/top_domains.json?hours=24&limit=5",
        "pipes/top_domains.json?hours=24&limit=5&account_id=" + ACCOUNT,
        f"pipes/domain_stats.json?domain={quote(domain)}&hours=24",
        "pipes/domain_stats.json?domain=nonexistent.example&hours=24",
        "pipes/hourly_stats.json?hours=24",
        "pipes/daily_stats.json?days=30",
        "pipes/error_distribution.json?hours=24",
        f"pipes/job_stats.json?job_id={job}",
        "pipes/job_stats.json?job_id=missing_job",
        "pipes/kpis.json?hours=24",
        "pipes/ai_usage.json?hours=24",
        "pipes/ai_usage.json?hours=24&account_id=" + ACCOUNT,
        f"pipes/job_timeline.json?job_id={job}",
        "pipes/job_timeline.json?job_id=missing_job",
        f"pipes/job_event_summary.json?job_id={job}",
        "pipes/account_usage.json?account_id=" + ACCOUNT + "&hours=24",
        "pipes/account_daily_usage.json?account_id=" + ACCOUNT + "&days=30",
        "pipes/account_daily_usage_by_operation.json?account_id=" + ACCOUNT + "&days=30",
        "pipes/api_key_usage.json?account_id=" + ACCOUNT + "&hours=24",
        # No account_id at all: each backend must default to the caller's.
        "pipes/account_usage.json?hours=24",
        "pipes/api_key_usage.json?hours=24",
    ]


def get(base: str, path: str, api_key: str):
    """(status, parsed JSON body or None for an empty body)."""
    request = urllib.request.Request(
        f"{base.rstrip('/')}/{path}", headers={"X-API-Key": api_key}
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            status, raw = response.status, response.read()
    except urllib.error.HTTPError as error:
        status, raw = error.code, error.read()
    try:
        return status, json.loads(raw) if raw else None
    except json.JSONDecodeError:
        return status, raw.decode("utf-8", "replace")


def normalize(body):
    """Drop statistics.elapsed, the only field allowed to differ."""
    if isinstance(body, dict) and isinstance(body.get("statistics"), dict):
        body = dict(body)
        stats = dict(body["statistics"])
        stats.pop("elapsed", None)
        body["statistics"] = stats
    return body


def account_of(rails: str, api_key: str) -> str:
    status, body = get(rails, "account", api_key)
    if status == 200 and isinstance(body, dict) and body.get("id"):
        return body["id"]
    status, body = get(rails, "analytics/v0/pipes/account_usage.json?hours=1", api_key)
    if status == 200 and isinstance(body, dict) and body.get("data"):
        return body["data"][0]["account_id"]
    sys.exit(
        f"cannot find the key's account (GET /account and account_usage both "
        f"failed: HTTP {status}); pass --account-id"
    )


def pretty(value) -> list[str]:
    return json.dumps(value, indent=1, sort_keys=True).splitlines()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--rails", required=True, help="Rails base URL")
    parser.add_argument("--engine", required=True, help="engine base URL")
    parser.add_argument("--api-key", required=True)
    parser.add_argument("--account-id")
    parser.add_argument("--job-id", default="job_test")
    parser.add_argument("--domain", default="example.com")
    args = parser.parse_args()

    account = args.account_id or account_of(args.rails, args.api_key)
    print(f"account: {account}")

    failures = 0
    selected = paths(args.domain, args.job_id)
    for path in selected:
        path = path.replace(ACCOUNT, account)
        full = f"analytics/v0/{path}"
        rails = get(args.rails, full, args.api_key)
        engine = get(args.engine, full, args.api_key)
        rails = (rails[0], normalize(rails[1]))
        engine = (engine[0], normalize(engine[1]))
        if rails == engine:
            print(f"  OK   {path}")
            continue
        failures += 1
        print(f"  DIFF {path}")
        diff = difflib.unified_diff(
            pretty(rails), pretty(engine), "rails", "engine", lineterm=""
        )
        for line in list(diff)[:60]:
            print("   " + line)

    if failures:
        print(f"\n{failures} of {len(selected)} paths differ", file=sys.stderr)
        return 1
    print(f"OK: {len(selected)} paths identical")
    return 0


if __name__ == "__main__":
    sys.exit(main())
