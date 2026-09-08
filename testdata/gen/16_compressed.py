#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the compressed twins of one small CSV (job key: compressed).

Run from the repo root:  python3 testdata/gen/16_compressed.py
Deterministic: gzip with mtime 0 and no name; bzip2 and xz carry no
timestamp. Standard library only.

WHY THIS FAMILY EXISTS
--------------------------------------------------------------------------
Monthly exports arrive zipped. `docs/design/2026-09-06-compressed-inputs.md`
chose to materialise a compressed file into a process-lifetime cache on
first touch and fingerprint the *compressed* bytes. These fixtures prove the
read: each twin must sniff to the same spec as the plain file, query to the
same sum, and reproduce under --frozen (the adversarial sweep picks them up
by itself). zstd is exercised in `src/fileio.rs`'s unit tests through the
crate's encoder, since Python may lack one.
--------------------------------------------------------------------------
FIXTURES  (all in testdata/, named compressed_*)
--------------------------------------------------------------------------
1. compressed_plain.csv      the twin: 4 rows of Datum;Region;Betrag, sum 4'460.00
2. compressed_plain.csv.gz   gzip of it
3. compressed_plain.csv.bz2  bzip2 of it
4. compressed_plain.csv.xz   xz of it
"""
import bz2
import gzip
import lzma
import os

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(REPO, "testdata")

BODY = (
    "Datum;Region;Betrag\n"
    "31.01.2025;Ost;1'100.00\n"
    "31.01.2025;West;1'110.00\n"
    "31.01.2025;Nord;1'120.00\n"
    "31.01.2025;Sued;1'130.00\n"
).encode("utf-8")


def note(path, what):
    print(f"wrote {os.path.relpath(path, REPO)} ({os.path.getsize(path)} bytes) - {what}")


def main():
    os.makedirs(OUT, exist_ok=True)
    plain = os.path.join(OUT, "compressed_plain.csv")
    with open(plain, "wb") as f:
        f.write(BODY)
    note(plain, "the plain twin")
    gz = os.path.join(OUT, "compressed_plain.csv.gz")
    with open(gz, "wb") as f:
        with gzip.GzipFile(fileobj=f, mode="wb", mtime=0, filename="") as g:
            g.write(BODY)
    note(gz, "gzip")
    bz = os.path.join(OUT, "compressed_plain.csv.bz2")
    with open(bz, "wb") as f:
        f.write(bz2.compress(BODY, 9))
    note(bz, "bzip2")
    xz = os.path.join(OUT, "compressed_plain.csv.xz")
    with open(xz, "wb") as f:
        f.write(lzma.compress(BODY, preset=6))
    note(xz, "xz")


if __name__ == "__main__":
    main()
