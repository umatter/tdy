# Changelog

Notable changes to `tdy` and `tdy-tui`. The two crates are versioned together.

## 0.2.0 — 2026-09-05

A correctness release. Two systematic audits of a 9,881-file corpus of real
public data found nine defects in 0.1.0 where tdy returned a plausible wrong
answer instead of a right one or a loud error — the single thing this tool
promises not to do. All nine are fixed, each with a regression test written
against the correct behaviour and a committed fixture reproducing the shape.

**Upgrading changes results on affected files, deliberately.** Row counts go
up where rows were being dropped; a spreadsheet money column that came back
`float64` now comes back `decimal`, which changes the Arrow schema downstream
code sees; and confidences and `notes` move. That is why this is 0.2.0 and
not 0.1.1.

**Existing sidecars are NOT invalidated by upgrading, and this matters here.**
A sidecar is fresh when the file's blake3 still matches; the tool version
that wrote it is recorded but never compared. So a `<file>.tdy.toml` written
by 0.1.0 keeps being used verbatim — including a spec carrying one of the
defects below, such as a `skip_rows` that eats real rows or a money column
typed `float64`. **To get these fixes on files you have already sniffed, you
must re-sniff them**: delete the sidecar and run `tdy sniff` again, or
`tdy fit` the target again for a planned pile. Sidecars marked
`method = "manual"` are yours and are left alone either way.

### Fixed — silent data loss

- **Rows dropped while hunting for a header.** The Excel sniffer chose the
  header as "the first row with ≥60% of the grid width populated, followed by
  a row with ≥50%", then skipped everything above it. Both bars are fractions
  of the *grid* width and the second is asked of the row *after* the
  candidate, so on a wide ragged sheet a header filling every column is
  rejected because the first data row under it is sparse — and the scan walks
  into the data until two consecutive rows happen to be dense enough, keeping
  the skip even after deciding the row it found was not a header. One audited
  workbook returned 61,952 of 62,168 rows, discarding seven universities,
  while reporting only "skipped 217 leading row(s) before the header". tdy now
  never skips past a row that is itself a header.
- **An ordinary last row deleted as a "total".** The footer check dropped a
  file's last row whenever any field matched a total-like label, with no
  corroboration. Where `subsector = "total"` is a routine category (2,288
  occurrences in one audited file), the last row — an ordinary record — was
  silently deleted. A trailing row is now treated as a summary row only when
  its label occurs at most once elsewhere in that column, corroborated against
  the sample's tail rather than a head-only probe.
- **A data record absorbed into a column name.** Header promotion fired
  whenever row 1 looked like a row of distinct labels, even when every row in
  the file has that shape — a path list, a `requirements.txt`, a single-line
  version file — deleting a record. Promotion is refused for a one-row table,
  and for a single-column table whose rows all share the first row's shape,
  with a doubt naming the ambiguity.

### Fixed — silent wrong values

- **Accented text returned as mojibake.** A file that is valid UTF-8 as a
  whole, but whose 4 KB tail sample begins mid multi-byte sequence, failed the
  "valid UTF-8 is UTF-8" check once head and tail were fused into one buffer,
  and fell through to charset detection, which settled on windows-1252 at
  confidence 0.80 with no warning. Head and tail are now checked
  independently.
- **Money read through binary floating point.** A spreadsheet column whose
  cells carry a *currency* number format is now typed `decimal`, not
  `float64`, so sibling money columns cannot disagree and results do not carry
  IEEE noise where the sheet shows cents. calamine does not expose the cell
  format, so `tdy` reads `xl/styles.xml` and the sheet XML from the workbook
  zip directly. The scale is the widest fractional part any sampled value
  actually has, so parsing at it never rounds a digit away; a currency column
  whose separator is ambiguous, or whose noise would need an implausible
  scale, is left as `float64` and says why rather than round a real value.
- **Interior aggregate rows counted as data.** The summary-row check only ever
  saw the file's *last* row, so anything below a `Total` — a blank line, a
  disclaimer, a legend block — hid it completely and it was read as an
  ordinary record, doubling every `sum()` over that column. Across 320 corpus
  files this now identifies 11 such rows, all genuine, none spurious. The row
  is reported rather than removed: which rows are safe to drop is a decision
  for a human with `drop_rows_matching`.
- **A late-starting column stayed text forever.** Whole-file type verification
  only widened a guess that had failed, and a text guess never fails, so a
  column whose first 500 rows are all null stayed text even when the rest of
  the file was clean numbers. Such a column is now typed from the whole file —
  and never narrowed to `float64`, which would have reintroduced float money.

### Added — warnings where tdy cannot be sure

- A legend or footnote block below the table, and a header repeated mid-file
  between concatenated sections, are both now reported. Neither is removed.
- A single-column all-text file gets a doubt naming *why* header and record
  are indistinguishable there, instead of the generic "no header row
  detected".

### Changed — warnings that were firing on correct output

- "Nearly every column typed as text; the layout may be misread" no longer
  fires when the file supplied its own column names. It was unearned on data
  that is simply textual — recipes, categories, episode notes, names — where
  it was the only thing pushing an otherwise correct parse below the
  escalation threshold, which is how people learn to ignore warnings.
- The trailing legend/footnote detector no longer claims a whole table. Its
  backward walk stops only at a full-width row, so on a sheet of uniformly
  sparse rows it ran to the top and advised deleting every row of data. A
  block must now have data above it and be mostly prose.

## 0.1.0 — 2026-09-03

First release.
