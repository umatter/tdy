# Workbook members

*2026-09-07. The decision `docs/design/2026-09-06-members-and-regions.md`
recommended, taken: a member is a path plus an optional sheet, discovered by
`fit` and locked. Steps 1 and 2 of that page's order; regions (step 3) stay
deferred, behind the review gate, on their own page when they come.*

---

## 1. What changes

A twelve-sheet workbook contributes twelve members to a dataset instead of
one, and the refusal `fit` gives today when several sheets produce the
declared table — `AmbiguousFrame`, naming them — becomes the expected case:
each fitting sheet is a member, each proved on its own, each named in the lock.
The corpus sweep refused 16 of 31 multi-sheet workbooks this way; the answer
those users want is *all of them, with the sheet as a column*, and
`source_name { from = "sheet" }` already turns the sheet into the column.

Three decisions, each the recommended option of the page above:

1. **Selector plus discovery**, in one PR as two commits. The selector alone
   is only reachable by hand-editing a lock, so it is landed first and proved
   that way, and discovery follows in the same review.
2. **One sidecar per sheet**: `book.xlsx#Q1.tdy.toml` beside the workbook.
   `book.xlsx.tdy.toml` keeps meaning what `messy('book.xlsx')` reads.
3. **Expansion by default.** Several fitting sheets become several members.
   The old behaviour was refusal, so nothing that worked before changes; the
   lock and the pile view name every member, so a `Data` and a `Data (copy)`
   both counting is visible rather than silent, and `exclude` removes one.

## 2. Identity

A lock member is

```toml
[[member]]
path = "2025.xlsx"
sheet = "Q1"          # absent for a plain member
blake3 = "…"
bytes = 12345
```

Two fields, never one string: a `#` in a file name or a sheet name must not
make the lock ambiguous. The textual form `2025.xlsx#Q1` exists where a person
reads or types a member — the report and the pile view, the `_member`
provenance column, `--accept` and `.accept`, `exclude` — and a typed reference
is **resolved against the members that exist**, never split by a rule: for
`a#b#c` the resolver tries `a#b` with sheet `c` and `a` with sheet `b#c` and
takes the one that names a member. Nothing else parses the string.

A workbook where exactly one sheet fits stays a **plain member**, the sheet
recorded in its spec's `sheet_name` as it is today, so existing locks, the
`sheet_frames_one_fits.xlsx` fixture and every single-sheet workbook keep their
meaning unchanged. Only several fitting sheets produce sheet members.

## 3. Sidecars and the lock

`sidecar_path(file, sheet)` is `<file>#<sheet>.tdy.toml`; `load`, `save`,
`spec_digest` and `hash_file`'s callers take the optional sheet, and the
single-argument forms remain as the `None` case. `SourceFingerprint` gains
`sheet` (optional, absent for a plain sidecar), so a sheet sidecar states what
it is about. Because the selector is part of the sidecar's *name*, `validate`,
`--stamp`, `.edit`, `$EDITOR` and the browser's companion folding need
nothing: a sheet sidecar is a `.tdy.toml` beside its file like any other.

**Drift stays per file.** The file's blake3 covers every sheet, so a renamed,
added, removed or edited sheet is `Changed` on that file — reported **once**,
not once per sheet member — and the refit rediscovers the sheet set. That is
the conservative direction the page asked for, and it needs no sheet listing
in the lock. `Duplicated` is on (path, sheet). `Removed` is a file that is
gone; `Added` is a file with no member at all.

`Lock::member(path)` becomes `Lock::member(path, sheet)`; acceptance carry-over
and `--accept` match on both.

## 4. Discovery

In `fit_pile`, a member file that is a workbook with more than one sheet is
first asked which sheets pass the cheap gates against the target
(`Rigour::Gates`, the same pass `fit_by_elimination` runs):

| fitting sheets | result |
|---|---|
| none | the ordinary gap report, for the ranked sheet — unchanged |
| one | one plain member, sheet in the spec — unchanged |
| several | one member per sheet, each fully fitted and proved on its own |

Every sheet member carries a note naming the sheets that did **not** fit, so
"of 5 sheets, 3 produce the declared table" is on the record. Sheets are
rediscovered on every fit, never read back from the previous lock:
membership comes from the fit and is locked, as it does for files.

The single-file `fit()` keeps refusing several fitting sheets with
`AmbiguousFrame`. "The spec for this file" has no single answer there, and
`tests/fit.rs::two_fitting_sheets_are_refused_not_ranked` keeps pinning it.
The pile is where the question "which members?" is asked, and it has a
different answer.

`exclude` keeps its file-glob meaning and additionally accepts exact member
references, applied after expansion: `exclude = '2025.xlsx#Cover'` removes one
sheet member and leaves the rest.

## 5. Everything downstream

- **`dataset()`** loads each member's sidecar by (path, sheet), re-proves
  conformance as today, and names the member textually in `_member`.
  Confinement is on the path; a sheet is not a path component.
- **The report** (`MemberReport`) keeps `path` as the textual member name —
  what `--accept` takes — and gains `sheet` for structure. `--json` and the
  MCP `fit`/`check` results therefore carry the sheet as a field.
- **The workbench** shows the member's own sheet in the raw head: the
  preview plumbing already takes a sheet, and the member view hands it the
  member's.
- **`draft`** is untouched: it drafts columns from files, and a sheet member
  is a fit-time discovery.

## 6. The rule this keeps

Membership is proved, not discovered at query time. `dataset()` reads sheet
members from the lock and never lists a workbook's sheets itself; a sheet
that appears or disappears changes the file's hash and is drift with a named
file. That is the property the whole layer exists for, and the one an
"expand the workbook on read" shortcut would have quietly cost.

## 7. Tests

Commit 1 (selector): a lock round-trips a sheet member; `Duplicated` is on
(path, sheet); `sidecar_path` with a sheet, and a sheet sidecar's fingerprint
states its sheet; a **hand-written** lock over `sheet_frames_two_fit.xlsx`
with two hand-written sheet sidecars queries to the sum of both sheets, with
`_member` naming `…#Q1` and `…#Q2`; drift after editing the workbook is one
`Changed` line.

Commit 2 (discovery): a pile holding `sheet_frames_two_fit.xlsx` fits to two
sheet members and the lock carries both, the dataset sums both;
`sheet_frames_one_fits.xlsx` stays one plain member; the non-fitting sheets
are named in the notes; `exclude = '…#Q2'` leaves one member; `--accept`
takes a textual reference; the JSON report carries `sheet`; the workbench's
member view for a sheet member shows that sheet's grid; the MCP server's
`fit` result names sheet members.
