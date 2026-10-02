#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the `json_records/` pile for tdy (job key: json-records).

Run from the repo root:  python3 testdata/gen/20_json_records.py

Deterministic and idempotent; stdlib only.

WHY THIS FAMILY EXISTS
--------------------------------------------------------------------------
About 7,900 of the corpus's 8,136 JSON files hold ONE object each:
villagerdb's items and villagers, `{id, name, category, games: {...}}`, one
file per thing. The record is the document and the table is the directory.
tdy declined a root object with no array in it, and read one that happened
to hold a small array (`games.nl.buyPrices`, one element) AS that array — a
one-row table of buy prices in place of the item.

This pile is modelled on those items, small enough to sum by hand:

  * some documents describe the item in `games.nh`, some in `games.nl`, one
    in both — so a target reaching `games/nh/sellPrice/value` through
    `OPTIONS(pointer = ...)` gets a value from some members and a null from
    the others;
  * one document (1-up-cap.json) holds arrays, as most corpus items do
    (`fashionThemes`, `sources`, `buyPrices` under `games.nl`): `tdy fit`
    must read it as one record by elimination — every array is tried
    against the target and fails — and never as a table of buy prices;
  * one document (lamp.json) writes its top-level keys in another order,
    which binding by name must not notice;
  * one document (fish-bait.json) names `games.nh` but no `sellPrice`: the
    pointer finds nothing and the nullable column is null.

--------------------------------------------------------------------------
FILES  (all in testdata/json_records/)
--------------------------------------------------------------------------

1-up-cap.json      Hats       nl sell 80; nl arrays fashionThemes, sources,
                              buyPrices [{bells, 320}]
acorn.json         Crafting   nh sell 200
apple.json         Food       nh sell 100, nl sell 100
bamboo-hat.json    Hats       nh sell 280
cardboard-box.json Furniture  nl sell 75
fish-bait.json     Tools      nh present, no sellPrice (null)
lamp.json          Furniture  nh sell 1200; keys written name, category, id, games

items.tdy.sql      the declared table: id, name, category NOT NULL;
                   nh_sell and nl_sell BIGINT through pointers, nullable.

GROUND TRUTH (over all 7 members)
  count(*)        = 7
  sum(nh_sell)    = 200 + 100 + 280 + 1200 = 1780   (4 non-null)
  sum(nl_sell)    = 80 + 100 + 75          = 255    (3 non-null)
  categories      = Crafting 1, Food 1, Furniture 2, Hats 2, Tools 1
"""

import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(REPO, "testdata", "json_records")


def bells(v):
    return {"currency": "bells", "value": v}


ITEMS = [
    (
        "1-up-cap",
        {
            "id": "1-up-cap",
            "name": "1-up Cap",
            "category": "Hats",
            "games": {
                "nl": {
                    "orderable": True,
                    "fashionThemes": ["Sporty"],
                    "sellPrice": bells(80),
                    "sources": ["Labelle"],
                    "buyPrices": [bells(320)],
                }
            },
        },
    ),
    ("acorn", {"id": "acorn", "name": "Acorn", "category": "Crafting", "games": {"nh": {"sellPrice": bells(200)}}}),
    (
        "apple",
        {
            "id": "apple",
            "name": "Apple",
            "category": "Food",
            "games": {"nh": {"sellPrice": bells(100)}, "nl": {"sellPrice": bells(100)}},
        },
    ),
    (
        "bamboo-hat",
        {"id": "bamboo-hat", "name": "Bamboo Hat", "category": "Hats", "games": {"nh": {"orderable": True, "sellPrice": bells(280)}}},
    ),
    (
        "cardboard-box",
        {"id": "cardboard-box", "name": "Cardboard Box", "category": "Furniture", "games": {"nl": {"sellPrice": bells(75)}}},
    ),
    ("fish-bait", {"id": "fish-bait", "name": "Fish Bait", "category": "Tools", "games": {"nh": {"orderable": False}}}),
    (
        "lamp",
        {"name": "Lamp", "category": "Furniture", "id": "lamp", "games": {"nh": {"sellPrice": bells(1200)}}},
    ),
]

TARGET = """\
-- villagerdb's items, in miniature: one JSON document per item, and the
-- directory is the table. Two prices live inside `games`, one per game; a
-- pointer says where, and a member that does not describe that game reads
-- null there.
CREATE TABLE items (
  id       TEXT   NOT NULL,
  name     TEXT   NOT NULL,
  category TEXT   NOT NULL,
  nh_sell  BIGINT OPTIONS(matches = 'games', pointer = '/nh/sellPrice/value'),
  nl_sell  BIGINT OPTIONS(matches = 'games', pointer = '/nl/sellPrice/value')
)
WITH (files = '*.json');
"""


def main():
    os.makedirs(OUT, exist_ok=True)
    for stem, doc in ITEMS:
        path = os.path.join(OUT, stem + ".json")
        with open(path, "w", encoding="utf-8", newline="") as f:
            json.dump(doc, f, indent=2)
            f.write("\n")
        print(f"wrote {os.path.relpath(path, REPO)} ({os.path.getsize(path)} bytes)")
    path = os.path.join(OUT, "items.tdy.sql")
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(TARGET)
    print(f"wrote {os.path.relpath(path, REPO)}")

    nh = sum(d["games"].get("nh", {}).get("sellPrice", {}).get("value", 0) for _, d in ITEMS)
    nl = sum(d["games"].get("nl", {}).get("sellPrice", {}).get("value", 0) for _, d in ITEMS)
    print(f"\nground truth: count = {len(ITEMS)}; sum(nh_sell) = {nh}; sum(nl_sell) = {nl}")


if __name__ == "__main__":
    main()
