# What is a member?

*2026-09-06. A sidecar is per file, so a twelve-sheet workbook contributes one
sheet to a dataset and a file holding three tables contributes one. The
operator catalogue's C9 (sheet half) and B10 are the same question seen from
two sides, and the question is about **identity**, not extraction. Steps 1 and 2 of §4 landed on 2026-09-07 — `docs/design/2026-09-07-workbook-members.md`; step 3 (regions) landed 2026-09-08 — `docs/design/2026-09-08-regions.md`.*

---

## 1. The two shapes, and how common they are

**A workbook with one sheet per period.** `tdy draft`'s sweep over the real
corpus refused 16 of 31 multi-sheet workbooks as `AmbiguousFrame` — correctly,
since many sheets produce the drafted table and nothing says which is meant.
That refusal is right and useless: the answer the user wants is *all of them,
with the sheet name as a column*.

**A file holding several tables.** Pollock's survey of 3,712 real-world CSVs
found **188 (5.1%)** containing multiple tables, "at times with preamble lines
or multiple header lines" — and that is CSV, where the layout is far rarer than
in spreadsheets. The research literature calls these *multiregion* files and
has published detection for them (Mondrian, PVLDB 2022).

Both are common. Neither is reachable.

## 2. Why this is an identity question

Everything in tdy is keyed on a path:

- `sidecar_path(file)` is `<file>.tdy.toml` — **one spec per file**
- `SourceFingerprint` is the blake3 of a file
- a lock member is a path plus a hash
- `fileio::confine` checks paths
- `provider::MessyFunc` opens a path

A workbook's second sheet has no path. Neither does a file's second table. So
supporting them means inventing a **member identity that is not a path**, and
then every one of those five places has to understand it.

That is the whole difficulty. Extraction already handles both cases —
`Extraction::Excel { sheet_name }` reads any sheet, and `range` reads any block.
What is missing is a way to *say which one* in a place that can hold more than
one answer per file.

## 3. Options

### A · A member is a path plus an optional selector

```
2025.xlsx#sheet=Januar
exports.csv#range=A10:F80
```

- Sidecars become `<file>#<selector>.tdy.toml`, or one sidecar holding several
  specs keyed by selector.
- The lock's member list gains the selector; the hash stays the file's.
- **The fingerprint gets weaker in an interesting way**: two members of the
  same workbook share a hash, so a change to sheet A invalidates the proof for
  sheet B. That is *conservative*, which is the right direction, but it means
  editing one sheet re-fits twelve.

### B · A member is a path, and a file expands to several members

`tdy fit` discovers the sheets/regions once and writes them into the lock as
distinct members with the same path and different selectors. `dataset()` reads
the lock and never re-discovers.

This is the same data model as A, but the *discovery* is a planning step whose
result is locked — which fits the existing rule that membership comes from the
lock and never from expanding a glob at query time. A new sheet appearing in a
workbook is then **drift**, named as such, which is exactly the behaviour a new
file already gets.

### C · Keep one member per file; make the selection declarable per file

A target's `files` gains a per-file selector, or a sidecar may name a sheet and
that is that. Twelve sheets means twelve entries in the declaration.

- **No new identity**, so nothing downstream changes.
- Unusable for the actual case: a twelve-sheet workbook needs twelve
  hand-written declarations, and a thirteenth sheet next year is silent.

## 4. Recommendation

**B**, and not before the sheet-name-as-a-column problem is solved — which
`source_name` now does (`from = "sheet"` already resolves the declared sheet).
The pieces then compose: `fit` expands a workbook into one member per sheet,
each member's spec names its sheet, and `source_name` turns that into a column
so the period is data rather than metadata.

The order that keeps each step honest:

1. **Selector in the lock** (`path` + `sheet`), with `dataset()` reading it.
   No discovery yet — a hand-written lock proves the model works.
2. **Discovery in `fit`**, with the ambiguity rule inverted: today several
   sheets fitting the target is `AmbiguousFrame`; under this model it is the
   *expected* case, and the error moves to "these sheets fit and these do not",
   which is a report rather than a refusal.
3. **Regions** (B10) last, and only with a detector whose output a human
   reviews. A region boundary is a judgement — Mondrian gets it right often,
   not always — so it belongs behind the review gate, unlike a sheet name,
   which the file states.

## 5. The rule this must not break

A dataset's membership is proved, not discovered at query time. Whatever a
member becomes, `dataset()` must still read it from the lock, and a sheet that
appears or disappears must be **drift** with a named file — not a silently
different row count. That is the property the whole layer exists for, and it is
the one an "expand the workbook at read time" shortcut would quietly cost.

## 6. What it touches

| | |
|---|---|
| `sidecar` | one spec per (path, selector) — the biggest change |
| `lockfile` | member identity, drift comparison |
| `fit` / `report::fit_pile` | discovery, and the inverted ambiguity rule |
| `dataset` | resolve a selector, not just a path |
| `fileio::confine` | a selector is not a path component; confinement must still be about the file |
| `target` | possibly nothing — `files` globs still name files |

## 7. Related

- `docs/design/2026-09-06-compressed-inputs.md` §6: a **zip of several CSVs** is
  the same question again, with the container being an archive rather than a
  workbook. If the identity model here lands, that becomes a third selector
  kind rather than a new design.
- `docs/design/2026-09-05-munging-taxonomy.md` **B10**, **C9**.
