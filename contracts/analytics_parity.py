#!/usr/bin/env python3
"""Diff the analytics pipes between two live backends (SCR-85 phase 3).

Queries every pipe on both backends back-to-back and compares the full JSON
response, normalizing only statistics.elapsed. Run with both backends up and
pointed at the same ClickHouse:

    python3 contracts/analytics_parity.py
    RUST_BASE=http://localhost:8080 RAILS_BASE=http://localhost:8081 \
        python3 contracts/analytics_parity.py
"""

import difflib
import json
import os
import sys
import urllib.request

RUST_BASE = os.environ.get("RUST_BASE", "http://localhost:8080")
RAILS_BASE = os.environ.get("RAILS_BASE", "http://localhost:8081")

ENDPOINTS = [
    "pipes",
    "pipes/top_domains.json?hours=24&limit=5",
    "pipes/domain_stats.json?domain=example.com&hours=24",
    "pipes/domain_stats.json?domain=nonexistent.example&hours=24",
    "pipes/hourly_stats.json?hours=24",
    "pipes/daily_stats.json?days=30",
    "pipes/error_distribution.json?hours=24",
    "pipes/job_stats.json?job_id=job_test",
    "pipes/job_stats.json?job_id=missing_job",
    "pipes/kpis.json?hours=24",
    "pipes/ai_usage.json?hours=24",
    "pipes/ai_usage.json?hours=24&account_id=acct_test",
    "pipes/job_timeline.json?job_id=job_test",
    "pipes/job_event_summary.json?job_id=job_test",
    "pipes/account_usage.json?account_id=acct_test&hours=24",
    "pipes/account_usage.json?account_id=missing_acct&hours=24",
    "pipes/account_daily_usage.json?account_id=acct_test&days=30",
    "pipes/account_daily_usage_by_operation.json?account_id=acct_test&days=30",
    "pipes/api_key_usage.json?account_id=acct_test&hours=24",
]


def fetch(base: str, path: str):
    with urllib.request.urlopen(f"{base}/analytics/v0/{path}") as response:
        return json.load(response)


def normalize(doc):
    if isinstance(doc, dict) and "statistics" in doc:
        doc = dict(doc)
        stats = dict(doc["statistics"])
        stats["elapsed"] = "NORMALIZED"
        doc["statistics"] = stats
    return doc


def main() -> int:
    failures = 0
    for endpoint in ENDPOINTS:
        rust = normalize(fetch(RUST_BASE, endpoint))
        rails = normalize(fetch(RAILS_BASE, endpoint))
        if rust == rails:
            print(f"  OK   {endpoint}")
            continue
        failures += 1
        print(f"  DIFF {endpoint}")
        rust_lines = json.dumps(rust, indent=1, sort_keys=True).splitlines()
        rails_lines = json.dumps(rails, indent=1, sort_keys=True).splitlines()
        for line in list(
            difflib.unified_diff(rust_lines, rails_lines, "rust", "rails", lineterm="")
        )[:40]:
            print("   " + line)

    if failures:
        print(f"\n{failures} of {len(ENDPOINTS)} endpoints differ")
        return 1
    print(f"\nPARITY OK ({len(ENDPOINTS)} endpoints compared)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
