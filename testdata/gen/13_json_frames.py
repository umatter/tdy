#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the `json_frames_*` fixtures for tdy (job key: json-frames).

Run from the repo root:  python3 testdata/gen/13_json_frames.py

Deterministic and idempotent; stdlib only.

WHY THIS FAMILY EXISTS
--------------------------------------------------------------------------
The corpus sweep's single largest source of "tdy was unsure" — thousands of
real files — is a JSON document holding SEVERAL arrays of records: an API
dump with `orders` and `audit_log`, a game database with a table per
concept. With nothing but the file, ranking them ("longest array of
objects") is a guess, and the sniffer correctly says so instead of choosing.

A declared table changes the problem. `tdy fit` tries the target against
*every* candidate array:

    exactly one fits  ->  proved by elimination, no guess anywhere
    several fit       ->  refused: two complete, well-typed, different
                          answers, and choosing would invent data provenance
    none fit          ->  the ordinary gap report

These fixtures are that trichotomy, minus the last case (any fixture with a
wrong column covers it).

--------------------------------------------------------------------------
FIXTURES  (all in testdata/, named json_frames_*)
--------------------------------------------------------------------------

1. json_frames_one_fits.json
   Four arrays: `meta.tags` (strings — not records), `customers`
   (name/city), `orders` (date/region/amount), `audit` (timestamp/user/what).
   Against a target declaring (day DATE, region TEXT, amount DECIMAL) only
   `/orders` fits. Ground truth: sum(amount) = 660.00 over 4 rows.

2. json_frames_two_fit.json
   `q1` and `q2`, byte-different data with the SAME shape — both fit the
   target, and the sums differ (600.00 vs 1500.00), so a planner that ranked
   one over the other would produce a well-typed, plausible, wrong number.
   Must be refused with both pointers named.
"""

import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(REPO, "testdata")


def write(name, doc, what):
    path = os.path.join(OUT, "json_frames_" + name)
    with open(path, "w", encoding="utf-8", newline="") as f:
        json.dump(doc, f, indent=1, sort_keys=True)
        f.write("\n")
    print(f"wrote {os.path.relpath(path, REPO)} ({os.path.getsize(path)} bytes) - {what}")


def build_one_fits():
    doc = {
        "meta": {"exported": "2025-09-01", "tags": ["monthly", "chf", "internal"]},
        "customers": [
            {"name": "Keller AG", "city": "Bern"},
            {"name": "Roth GmbH", "city": "Basel"},
        ],
        "orders": [
            {"day": "2025-08-05", "region": "Ost", "amount": "150.00"},
            {"day": "2025-08-12", "region": "West", "amount": "160.00"},
            {"day": "2025-08-19", "region": "Nord", "amount": "170.00"},
            {"day": "2025-08-26", "region": "Sued", "amount": "180.00"},
        ],
        "audit": [
            {"at": "2025-08-05T10:00:00", "user": "hr", "what": "export"},
            {"at": "2025-08-31T16:30:00", "user": "hr", "what": "close"},
        ],
    }
    write("one_fits.json", doc, "only /orders fits (day, region, amount); sum 660.00")


def build_two_fit():
    def rows(base):
        return [
            {"day": f"2025-0{m}-28", "region": r, "amount": f"{base + 10 * i}.00"}
            for i, (m, r) in enumerate([(1, "Ost"), (2, "West"), (3, "Nord")])
        ]

    doc = {"q1": rows(190), "q2": rows(490)}
    write(
        "two_fit.json",
        doc,
        "q1 (sum 600.00) and q2 (sum 1500.00) BOTH fit: must be refused",
    )


def main():
    os.makedirs(OUT, exist_ok=True)
    build_one_fits()
    build_two_fit()
    q1 = sum(190 + 10 * i for i in range(3))
    q2 = sum(490 + 10 * i for i in range(3))
    orders = sum(150 + 10 * i for i in range(4))
    print(f"\nground truth: orders = {orders}.00; q1 = {q1}.00, q2 = {q2}.00")


if __name__ == "__main__":
    main()
