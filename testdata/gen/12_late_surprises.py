#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the `late_surprise_*` fixtures for tdy (job key: late-surprises).

Run from the repo root:  python3 testdata/gen/12_late_surprises.py

Deterministic and idempotent; stdlib only.

WHY THIS FAMILY EXISTS
--------------------------------------------------------------------------
These four shapes are not invented. Each is the reduction of a file that made
tdy die mid-query, found by running it over `corpus/` — twenty-six public
data-wrangling repositories fetched by `scripts/download_corpus.sh`, 9,881 real
files nobody wrote for us.

That corpus is gitignored and CI never sees it, so the *shapes* live here
instead. Without them the fixes are protected only on a machine that happens to
have seven gigabytes of somebody else's homework on it.

Every one of these has the same character: **the first 500 rows lie.** tdy
types a column from a sample and the sample is honest about itself and wrong
about the file. The failure was never a wrong value — tdy errored, correctly,
naming the row — but "correctly refused" is not the same as "works", and a
tool that refuses one real CSV in ten is not one you would reach for.

    upstream file                          what broke                    row
    -------------------------------------  ----------------------------  ------
    datascience-box hotels.csv             `children` is `NA`            40,601
    dlab animalRescue.csv                  id gains a `-18112015` suffix   4,067
    divvy 202306-tripdata.csv              id becomes `TA1309000067`         708
    lc-openrefine solar-patents.csv        a second export's header        1,001

--------------------------------------------------------------------------
FIXTURES  (all in testdata/, all named late_surprise_*)
--------------------------------------------------------------------------

1. late_surprise_na_after_the_sample.csv
   `children` is an integer for 900 rows and then `NA` at row 901. tdy already knew `NA`
   means missing — it is in `sniff::NA_TOKENS` — but only declared it when it
   happened to *see* one inside the sample, so the identical file with the
   `NA` near the top read fine and this one did not. The same file behaving
   two ways depending on where a token sits is worse than either answer.
   Correct parse: 1000 rows; `children` Int64 with one null at row 901;
   count(children) = 999 non-null, sum(children) = 999.

2. late_surprise_id_turns_alphanumeric.csv
   `station_id` is digits for 700 rows, then `TA1309000067`. No vocabulary
   fixes this — the value is data, not a missing marker — so the only honest
   answer is that the column is text.
   Correct parse: 1000 rows, `station_id` Utf8, and a note naming the offending
   value, its row, and how many there are out of how many.

3. late_surprise_second_export_header.csv
   Two exports concatenated: at row 501 the file contains another header row.
   Every column is text there, so a numeric column dies on it.
   Correct parse: `amount` Utf8 with a note; the junk row is *kept*, because
   dropping rows that fail to parse is precisely the silent data loss this
   project refuses. A human who agrees it is a stray adds a
   `drop_rows_matching` and narrows the type.

4. late_surprise_repeated_header.csv
   The same thing, but the repeated header is byte-identical to the first.
   That one tdy *can* settle without judgement — a row that reproduces the
   header exactly is provably not data — so it is dropped automatically and
   the numeric columns keep their types.
   Correct parse: 1000 rows (the repeated header gone), `amount` Int64,
   sum(amount) = 600500.

The difference between 3 and 4 is the whole point of the pair. Both look like
"a header in the middle of the file". Only one of them can be recognised as
such without guessing, and tdy does exactly the one it can prove.
"""

import os

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(REPO, "testdata")
PREFIX = "late_surprise_"


def note(path, what):
    print(f"wrote {os.path.relpath(path, REPO)} ({os.path.getsize(path)} bytes) - {what}")


def write(name, text, what):
    path = os.path.join(OUT, PREFIX + name)
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(text)
    note(path, what)


def build_na_after_the_sample():
    """An NA far past the type sample, in an otherwise clean integer column."""
    rows = ["id,children,city"]
    for i in range(1, 1001):
        # Exactly one child per row, so the sum is checkable by hand, and one
        # NA at row 901 — well past the 500-row type sample.
        children = "NA" if i == 901 else "1"
        rows.append(f"{i},{children},Zurich")
    write(
        "na_after_the_sample.csv",
        "\n".join(rows) + "\n",
        "NA at row 901; 999 non-null of 1000 rows",
    )


def build_id_turns_alphanumeric():
    """A numeric id that stops being numeric, far past the sample."""
    rows = ["trip_id,station_id,minutes"]
    for i in range(1, 1001):
        station = "TA1309000067" if i in (701, 845) else str(600000 + i)
        rows.append(f"{i},{station},{i % 60}")
    write(
        "id_turns_alphanumeric.csv",
        "\n".join(rows) + "\n",
        "station_id is digits until row 701; 2 of 1000 are not",
    )


def build_second_export_header():
    """Two exports concatenated, the second header worded differently."""
    rows = ["invoice,amount,customer"]
    for i in range(1, 501):
        rows.append(f"{i},{100 + i},K{i:04d}")
    # The second export's header: same shape, different words.
    rows.append("Invoice number,Total,Client")
    for i in range(501, 1001):
        rows.append(f"{i},{100 + i},K{i:04d}")
    write(
        "second_export_header.csv",
        "\n".join(rows) + "\n",
        "a differently-worded header at row 501: NOT droppable without judgement",
    )


def build_repeated_header():
    """The same, but the repeat is byte-identical to the real header."""
    header = "invoice,amount,customer"
    rows = [header]
    for i in range(1, 501):
        rows.append(f"{i},{100 + i},K{i:04d}")
    rows.append(header)  # provably not data
    for i in range(501, 1001):
        rows.append(f"{i},{100 + i},K{i:04d}")
    write(
        "repeated_header.csv",
        "\n".join(rows) + "\n",
        "an identical header at row 501: droppable, and dropped",
    )


def main():
    os.makedirs(OUT, exist_ok=True)
    build_na_after_the_sample()
    build_id_turns_alphanumeric()
    build_second_export_header()
    build_repeated_header()
    # Printed rather than asserted from memory: the docstring above first
    # claimed 900 and 350750, both wrong, and the generator is what settled it.
    kids = sum(1 for i in range(1, 1001) if i != 901)
    amount = sum(100 + i for i in range(1, 1001))
    print(
        f"\nground truth: 1000 data rows each; "
        f"sum(children) = {kids} over {kids} non-null; sum(amount) = {amount}"
    )


if __name__ == "__main__":
    main()
