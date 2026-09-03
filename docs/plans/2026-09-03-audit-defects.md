# Audit Defects Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the defects a 148-file verified audit of the real corpus found, so that
tdy either reads these files correctly or fails loudly — never silently returns a wrong
table.

**Architecture:** Two tiers. **Tier 1** defects have an unambiguous correct behaviour and
get fixed properly (encoding, header promotion, footer heuristic, type narrowing, money
typing). **Tier 2** defects are structural readings tdy cannot reliably get right
(multi-row footnote blocks, stacked/repeated headers, newline-in-cell); for those the
correct fix is *detection and a loud low-confidence refusal to claim certainty*, which is
what the tool's governing rule actually demands. Every defect gets a generated fixture and
a regression test written against the correct behaviour.

**Tech Stack:** Rust; `src/sample.rs` (encoding), `src/sniff.rs` (header/footer/probe),
`src/engine.rs` (Excel extraction), `testdata/gen/13_audit_defects.py` (fixtures),
`tests/regression.rs`.

**Spec:** `gap_reports/AUDIT_FINDINGS.md` — the audit that found these, with per-file
evidence. Root causes below were traced in the code, not guessed.

## Global Constraints

- Commit messages end with exactly these two trailer lines:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_01DmEku7uNkLUeyiNE38sho8`
- **Never use `git stash`** (shared stash stack across worktrees). Use `git show <rev>:<path>`.
- CI runs `cargo clippy --all-targets -- -D warnings`; clippy is NOT installed locally.
  Write clippy-clean: `writeln!` not `write!("...\n")`, ≤7 params per function.
- Test with `cargo test --workspace --lib --tests`.
- **Fixtures are generated, never hand-written**: add generators to
  `testdata/gen/13_audit_defects.py`, run `python3 gen_fixtures.py 13`. Zip writers must
  pin entry timestamps and patch `dcterms:modified` (see CLAUDE.md) or sidecar hashes go stale.
- The governing rule: a fix that makes tdy *quietly* produce a different wrong answer is
  not a fix. When in doubt, lower confidence and name the problem in a note.
- Regression tests go in `tests/regression.rs`, one per defect, named for the defect.

---

### Task 1: Encoding — a torn character in the tail sample must not defeat the UTF-8 check

**Files:**
- Modify: `src/sample.rs` (`detect_encoding` ~line 73; the head+tail concatenation ~line 220)
- Test: `src/sample.rs` unit tests (~line 390), `tests/regression.rs`

**Root cause (traced):** `build` concatenates `head + tail` and calls `detect_encoding` on
the pair so the guess sees every sampled byte. `read_head_tail` seeks the tail to an
arbitrary byte offset, so the tail can begin **mid multi-byte sequence**. `std::str::from_utf8`
then fails on the concatenation, the "valid UTF-8 is UTF-8" short-circuit is skipped, and
chardetng guesses `windows-1252`. Confirmed on the real file: `country_subdivisions.csv` is
valid UTF-8 whole-file, but its last 4096 bytes begin with a continuation byte
("invalid start byte" at offset 0), and tdy records `encoding = "windows-1252"` at
confidence 0.80 with no warning — every accented value comes back as mojibake
(`Sant Julià de Lòria` → `Sant JuliÃ  de LÃ²ria`).

- [ ] **Step 1: Write the failing unit test** in `src/sample.rs`'s test module:

```rust
#[test]
fn a_torn_multibyte_char_at_a_sample_boundary_is_still_utf8() {
    // A tail sample that begins in the middle of "à" (0xC3 0xA0): the leading
    // 0xA0 is a continuation byte, so the buffer is not valid UTF-8 — but the
    // FILE is, and freezing windows-1252 here mangles every accented value.
    let head = "code,name\nAD-06,Sant Juli".as_bytes();
    let torn_tail = &[0xA0u8, b' ', b'd', b'e', b' ', b'L', 0xC3, 0xB2, b'r', b'i', b'a', b'\n'];
    let mut both = head.to_vec();
    both.extend_from_slice(torn_tail);
    assert!(std::str::from_utf8(&both).is_err(), "precondition: buffer is torn");
    assert_eq!(detect_encoding(&both).name(), "UTF-8");
}

#[test]
fn a_truncated_trailing_sequence_is_still_utf8() {
    // The head can end mid-character too.
    let mut b = "Zürich, Genève, Basel".as_bytes().to_vec();
    b.truncate(b.len() - 1); // cut the last byte of "è"
    assert_eq!(detect_encoding(&b).name(), "UTF-8");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tdy --lib sample::` — expect both new tests to FAIL (they return
windows-1252 or another guess).

- [ ] **Step 3: Implement.** In `detect_encoding`, replace the strict validity check with
one that tolerates truncation artifacts at either boundary, keeping the UTF-16 check first:

```rust
    // Valid UTF-8 is UTF-8. chardetng is a statistical guesser and will
    // cheerfully call a short ASCII file windows-1252; freezing that guess
    // then mangles every multi-byte character later in the file.
    //
    // The bytes handed to us are a SAMPLE: `build` concatenates a head with a
    // tail read from an arbitrary offset, so a multi-byte character can be torn
    // at the tail's start and another truncated at the very end. Neither says
    // anything about the file's encoding, and treating a torn sample as
    // "not UTF-8" is how a valid UTF-8 file acquired windows-1252 mojibake.
    if is_utf8_apart_from_torn_edges(bytes) {
        return encoding_rs::UTF_8;
    }
```

and add the helper beside it:

```rust
/// Is this sample valid UTF-8 once truncation at its edges is discounted?
///
/// Skips leading continuation bytes (0x80..=0xBF, at most 3 — a torn character
/// at the start of a tail sample) and tolerates an incomplete sequence at the
/// very end (`Utf8Error::error_len() == None`, which means "valid so far, ran
/// out of bytes"). An *interior* error is a real signal and returns false.
fn is_utf8_apart_from_torn_edges(bytes: &[u8]) -> bool {
    let start = bytes.iter().take(3).take_while(|b| (0x80..=0xBF).contains(*b)).count();
    match std::str::from_utf8(&bytes[start..]) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && e.valid_up_to() > 0,
    }
}
```

- [ ] **Step 4: Run the tests** — `cargo test -p tdy --lib sample::` — both PASS, and the
existing `detect_encoding` tests (ASCII, `Zürich`, `M\xfcller`, the windows-1252 fixture at
~line 406) still pass. The `M\xfcller` case must still NOT be UTF-8: 0xFC is an invalid
start byte in interior position, so `error_len()` is `Some(1)`.

- [ ] **Step 5: Add the end-to-end regression test** in `tests/regression.rs`, using a
generated fixture (Task 7 creates it; write the test now against the intended name):

```rust
/// A valid-UTF-8 file whose tail sample begins mid-character was recorded as
/// windows-1252 and every accented value came back as mojibake, at confidence
/// 0.80 with no warning. Found by the 2026-09-03 corpus audit.
#[test]
fn utf8_file_with_a_torn_tail_sample_is_not_mojibake() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("torn_tail_utf8.csv");
    std::fs::copy("testdata/torn_tail_utf8.csv", &f).unwrap();
    let spec = tdy::sniff::sniff(&f, &Default::default(), Default::default()).unwrap();
    assert_eq!(spec.extraction.encoding.as_deref(), Some("utf-8"), "{:?}", spec.extraction);
    let t = tdy::engine::execute(&spec, &f, Default::default()).unwrap();
    let joined = format!("{t:?}");
    assert!(!joined.contains('\u{c3}'), "mojibake in output: {joined}");
}
```

Adapt the call shapes to the real `sniff`/`execute` signatures in this repo (check
`tests/regression.rs` neighbours); the assertions are the contract.

- [ ] **Step 6: Commit**

```bash
git add src/sample.rs tests/regression.rs
git commit -m "a torn character in a sample must not cost a file its encoding"
```

---

### Task 2: Header promotion must not consume the only, or the first, record

**Files:**
- Modify: `src/sniff.rs` (`header_verdict` / the `HeaderVerdict::Present` arm ~line 408)
- Test: `src/sniff.rs` unit tests, `tests/regression.rs`

**Root cause (traced):** `header_verdict` promotes row 1 whenever it looks like a row of
distinct labels. On a file whose rows are all structurally identical — a list of file
paths, a pip requirements list, a one-line version file — every row "looks like" a header,
so row 1 is consumed into the column name and **a data record disappears**. Confirmed:
`SOLUTION_FILES.txt` (168 paths → `count(*)` 167), `requirements.txt`
(`numpy==1.22.0` absorbed), `workflows-version.txt` (a single line `1.0.1` → **an empty
table**, no warning).

- [ ] **Step 1: Write the failing tests** in `tests/regression.rs`:

```rust
/// A single-line file has no header — promoting its only line returned an
/// EMPTY table with no warning. Found by the 2026-09-03 corpus audit.
#[test]
fn a_single_line_file_keeps_its_only_row() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("one_line.txt");
    std::fs::write(&f, "1.0.1\n").unwrap();
    let spec = tdy::sniff::sniff(&f, &Default::default(), Default::default()).unwrap();
    let t = tdy::engine::execute(&spec, &f, Default::default()).unwrap();
    assert_eq!(t.num_rows(), 1, "the file's only datum was consumed as a header");
}

/// Every row structurally identical = no header. Promoting row 1 dropped a
/// record into the column name.
#[test]
fn a_homogeneous_single_column_list_keeps_its_first_row() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("paths.txt");
    std::fs::write(&f, "a/b/one.csv\na/b/two.csv\na/b/three.csv\na/b/four.csv\n").unwrap();
    let spec = tdy::sniff::sniff(&f, &Default::default(), Default::default()).unwrap();
    let t = tdy::engine::execute(&spec, &f, Default::default()).unwrap();
    assert_eq!(t.num_rows(), 4, "row 1 was consumed as a header");
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --test regression single_line` and
`homogeneous_single_column` both FAIL (1 row becomes 0; 4 becomes 3).

- [ ] **Step 3: Implement.** In `sniff.rs`, before the `HeaderVerdict::Present` promotion,
refuse promotion in the two cases where it provably destroys data:

```rust
    // A header is a row that DESCRIBES the rows under it. Two shapes prove
    // there is nothing to describe, and promoting anyway deletes a record:
    //   - a table with one row (there is nothing below the "header"), and
    //   - a single-column table whose every row has the same shape (a list of
    //     paths, a requirements file): each row is as much a "label" as row 1,
    //     so "looks like a header" cannot distinguish them.
    // Found by the 2026-09-03 corpus audit: a one-line version file returned
    // an EMPTY table, and a 168-path list returned 167 rows.
    let promotable = table.rows.len() > 1
        && !(table.width() <= 1 && rows_are_homogeneous(&table.rows));
```

and gate the `Present` arm on `promotable`, falling through to a doubt when it is false:

```rust
        HeaderVerdict::Present if !promotable => {
            doubts.add(
                0.2,
                "every row here has the same shape, so no row describes the others; \
                 read as data with generated column names. If the first row really is \
                 a header, add promote_header to the sidecar.",
            );
        }
```

Add the helper (pure, unit-testable):

```rust
/// Do all rows have the same shape — same field count, and no row that is
/// alone in being non-numeric or alone in being numeric? Used to recognise a
/// list (paths, versions, requirements) where no row is a header.
fn rows_are_homogeneous(rows: &[Vec<String>]) -> bool {
    let Some(first) = rows.first() else { return true };
    rows.iter().all(|r| r.len() == first.len())
}
```

- [ ] **Step 4: Run the tests** — both new tests PASS; the whole suite still passes.
Pay attention to `tests/fixtures.rs` and `tests/formats.rs`: any fixture that is a
legitimate single-column table WITH a header will now be read differently. If one breaks,
that is a real behaviour change — record it in the ledger and check the fixture's intent
before adjusting either side.

- [ ] **Step 5: Commit**

```bash
git add src/sniff.rs tests/regression.rs
git commit -m "a header describes the rows below it; a list has none"
```

---

### Task 3: The footer heuristic must corroborate before deleting a row

**Files:**
- Modify: `src/sniff.rs` (`footer_rows` ~line 879, `footer_row_cells` ~line 874)
- Test: `src/sniff.rs` unit tests (~line 1613), `tests/regression.rs`

**Root cause (traced):** `footer_rows` drops the last row when *any* field matches
`FOOTER_FIELD` (`^(total|totals|sum|…)$`). It inspects only the last line and requires no
corroboration. Confirmed: `state_retail.csv` has `subsector = "total"` as a routine
category appearing **2,288 times**; the file's last row is an ordinary Wyoming
August-2022 record, and tdy **silently deletes it** (96 WY/2022 rows in the file, 95 from
a query). No warning, no confidence penalty.

- [ ] **Step 1: Write the failing test** in `tests/regression.rs`:

```rust
/// "total" as a routine category value in the last row is not a summary row.
/// tdy silently deleted a real record. Found by the 2026-09-03 corpus audit.
#[test]
fn a_frequent_category_value_is_not_a_footer() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("category_total.csv");
    let mut s = String::from("state,subsector,sales\n");
    for i in 0..40 {
        s.push_str(&format!("S{i},retail,{}\n", 100 + i));
        s.push_str(&format!("S{i},total,{}\n", 200 + i));
    }
    std::fs::write(&f, &s).unwrap();
    let spec = tdy::sniff::sniff(&f, &Default::default(), Default::default()).unwrap();
    let t = tdy::engine::execute(&spec, &f, Default::default()).unwrap();
    assert_eq!(t.num_rows(), 80, "a routine 'total' category row was dropped as a footer");
}
```

- [ ] **Step 2: Run to verify failure** — expect 79 rows, not 80.

- [ ] **Step 3: Implement.** `footer_rows` currently sees only the last line. Give it the
column of values it is judging against, and require the label to be *rare* in that column:

```rust
/// A trailing summary row, if the evidence supports it.
///
/// The label alone is not evidence: `subsector = "total"` is a routine category
/// in some exports, and dropping the last row because of it silently deleted a
/// real record (2026-09-03 corpus audit). A summary row is a row whose label is
/// EXCEPTIONAL — it appears in the last row and (almost) nowhere else in that
/// column. `column_values` is that column over the sampled rows.
fn footer_rows(last_line: Option<&str>, delimiter: Option<char>, sampled: &[Vec<String>]) -> u32 {
```

Inside, after finding which field index matched, count how often the same (case-folded)
value occurs in that field across `sampled`; treat the row as a footer only when the
count is `<= 1` (the last row itself). Keep the existing `FOOTER_LINE` behaviour for the
delimiter-less case. Update the three existing unit tests at ~line 1613 to pass a
`sampled` slice (a single occurrence, so they keep their current expectations), and add:

```rust
    #[test]
    fn a_repeated_label_is_a_category_not_a_footer() {
        let rows: Vec<Vec<String>> = (0..10)
            .map(|i| vec![format!("S{i}"), "total".into(), "1".into()])
            .collect();
        assert_eq!(footer_rows(Some("S9,total,1"), Some(','), &rows), 0);
        let once: Vec<Vec<String>> = vec![vec!["S1".into(), "retail".into(), "1".into()]];
        assert_eq!(footer_rows(Some("Total,,14337.00"), Some(','), &once), 1);
    }
```

- [ ] **Step 4: Run the tests** — the new tests pass; the existing footer tests still pass.

- [ ] **Step 5: Commit**

```bash
git add src/sniff.rs tests/regression.rs
git commit -m "a label that repeats down a column is a category, not a total"
```

---

### Task 4: Whole-file type verification must narrow, not only widen

**Files:**
- Modify: `src/sniff.rs` (`verify_types` and the probe-derived typing)
- Test: `tests/regression.rs`

**Root cause (traced):** types are guessed from the first `PROBE_ROWS = 2000` rows and then
verified against the whole file — but verification only *widens* a guess that fails. A
column whose values all appear after row 2000 is all-null in the probe, guesses `Utf8`,
and a text guess never fails, so it is never corrected. Confirmed:
`individual_results_df.csv` types `p7` as text while `p1`–`p6` (identical shape) are
whole numbers; `p7`'s only non-null values sit at rows 21,513–21,655.

- [ ] **Step 1: Write the failing test** in `tests/regression.rs`:

```rust
/// A column whose values all appear after the probe window was typed text
/// while identical sibling columns were numeric, and whole-file verification
/// never corrected it (it only widens). Found by the 2026-09-03 corpus audit.
#[test]
fn a_column_that_starts_late_is_still_typed() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("late_column.csv");
    let mut s = String::from("a,b\n");
    for i in 0..2500 { s.push_str(&format!("{i},NA\n")); }
    for i in 0..50 { s.push_str(&format!("{i},{}\n", i % 9)); }
    std::fs::write(&f, &s).unwrap();
    let spec = tdy::sniff::sniff(&f, &Default::default(), Default::default()).unwrap();
    let b = spec.columns.iter().find(|c| c.name == "b").expect("column b");
    assert!(!matches!(b.dtype, tdy::spec::DType::Utf8),
            "late-starting numeric column stayed text: {:?}", b.dtype);
}
```

- [ ] **Step 2: Run to verify failure** — `b` is `Utf8`.

- [ ] **Step 3: Implement.** In the whole-file verification pass, when a column was typed
`Utf8` **and every value seen in the probe was null/NA**, re-derive its type from the
values the whole-file pass actually saw, and narrow it if they are uniformly typeable.
Guard it tightly: narrow only from `Utf8`, only when the probe evidence was empty, and only
when the whole-file evidence is unanimous — never narrow a column that had real text in it.
Record a note when narrowing happens, so the change is visible in the sidecar.

- [ ] **Step 4: Run the tests** — new test passes; `tests/fixtures.rs`'s pinned dtypes and
the `late_surprise_*` fixtures still pass (those exercise widening, which must be untouched).

- [ ] **Step 5: Commit**

```bash
git add src/sniff.rs tests/regression.rs
git commit -m "a column that only starts late still gets a type"
```

---

### Task 5: Trailing prose blocks and repeated headers must be detected and said out loud

**Files:**
- Modify: `src/sniff.rs` (`frame_excel_sheet` ~line 520 and the delimited path's doubts)
- Test: `tests/regression.rs`

**Root cause (traced):** the footer logic handles *one* trailing summary row. Government
statistical spreadsheets end with a legend or footnote block of many rows, and repeat their
header mid-sheet between sections. tdy reads all of it as data. Confirmed: 5 footnote-block
cases (`ttb_brewery_size_2017/2018/2011.xlsx`, `ttb_brewery_state_2008-2019.xlsx`,
`supplemental-table5.xlsx` — `count(*)` 27 vs 13, 60 vs 51, 182 vs 178) and 7 stacked or
repeated-header cases (`h02b/h03b/h01w/h17.xlsx`, `WealthbyRace.xlsx`, `bls-2020.xlsx`, the
NASS CSVs). `ttb_brewery_size_2018.xlsx` did this at confidence **0.80 with no note**.

**This task does NOT attempt to parse these files correctly** — reconstructing a stacked
header is out of scope. The correct behaviour under the governing rule is to *notice* and
*say so*, so the number is never trusted silently.

- [ ] **Step 1: Write the failing tests** in `tests/regression.rs`:

```rust
/// A legend/footnote block below the data was read as data rows at high
/// confidence with no warning. Found by the 2026-09-03 corpus audit.
#[test]
fn a_trailing_prose_block_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("footnote_block.csv");
    let mut s = String::from("size,breweries,barrels\n");
    for i in 0..12 { s.push_str(&format!("band {i},{},{}\n", i * 3, i * 1000)); }
    s.push_str("\nLegend\n");
    s.push_str("1) Number of Breweries - Count of brewery premises reporting operations.\n");
    s.push_str("2) Size - Based on Annual Production as reported on the operations report.\n");
    std::fs::write(&f, &s).unwrap();
    let spec = tdy::sniff::sniff(&f, &Default::default(), Default::default()).unwrap();
    assert!(spec.confidence.unwrap_or(1.0) < 0.8, "confidence {:?}", spec.confidence);
    assert!(spec.notes.iter().any(|n| n.contains("trailing")),
            "no note about the trailing block: {:?}", spec.notes);
}

/// The header repeated mid-file between sections was read as data.
#[test]
fn a_repeated_header_row_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("repeated_header.csv");
    let mut s = String::from("year,lowest,second\n");
    for i in 0..10 { s.push_str(&format!("{},{},{}\n", 2000 + i, i * 10, i * 20)); }
    s.push_str("year,lowest,second\n");
    for i in 0..10 { s.push_str(&format!("{},{},{}\n", 2010 + i, i * 11, i * 21)); }
    std::fs::write(&f, &s).unwrap();
    let spec = tdy::sniff::sniff(&f, &Default::default(), Default::default()).unwrap();
    assert!(spec.notes.iter().any(|n| n.contains("repeat")),
            "no note about the repeated header: {:?}", spec.notes);
}
```

- [ ] **Step 2: Run to verify failure** — no such notes exist; confidence stays high.

- [ ] **Step 3: Implement two detectors**, both operating on the sampled rows after header
promotion, each adding a doubt (never silently dropping rows — the user decides with
`skip_rows`/`drop_rows_matching`):

1. **Trailing prose block.** Walk backwards from the last row while a row is blank, or has
   fewer populated fields than the table's width and its first populated cell is long free
   text (say > 40 chars with a space) or matches `^(legend|note|notes|footnote|source)s?\b`.
   If ≥ 2 such rows, add a doubt naming the count and the first offending row number, and
   suggest `skip_rows` with that tail count.
2. **Repeated header.** After promotion, count rows equal (case- and whitespace-folded) to
   the promoted header. If ≥ 1, add a doubt naming how many and the first row number, and
   suggest `drop_rows_matching`. A byte-identical repeated header is already handled
   elsewhere for the streaming path — reuse that comparison if it is reachable.

Both doubts must lower confidence enough to fall under the 0.8 escalation threshold — that
is the entire point: `ttb_brewery_size_2018.xlsx` reported 0.80 with no note.

- [ ] **Step 4: Run the tests** — new tests pass; sweep for regressions with
`cargo test --workspace --lib --tests`, and re-check `tests/corpus.rs` expectations only if
`TDY_CORPUS` is set (it is not in CI).

- [ ] **Step 5: Commit**

```bash
git add src/sniff.rs tests/regression.rs
git commit -m "a legend below the table and a header repeated inside it both get said out loud"
```

---

### Task 6: Excel — a newline inside a cell is not a row boundary; money types consistently

**Files:**
- Modify: `src/engine.rs` (Excel extraction / `render_cell`), `src/sniff.rs` (Excel typing)
- Test: `tests/regression.rs`, fixture from Task 7

**Root cause (traced):** two independent Excel defects.
(a) `ttb_monthly_stats_2018-12.xlsx` cell A10 contains an embedded newline
(`"Manufacture\nOf Beer (In Barrels)"`); tdy emits it as **two table rows**, attaching the
row's numeric values to the first fragment and leaving an orphan blank row.
(b) `PCA_Report_FY16Q3.xlsx` types `added` as `decimal(2)` but five sibling currency columns
— identical `"$"#,##0.00` number formats — as `float64`, so results carry IEEE noise
(`255871181.10999995` where the file says `255871181.11`), violating the documented
"money becomes decimal" rule.

- [ ] **Step 1: Write the failing tests** in `tests/regression.rs` against the generated
fixtures from Task 7 (`testdata/xl_cell_newline.xlsx`, `testdata/xl_money_siblings.xlsx`):

```rust
/// A newline inside one spreadsheet cell was emitted as two table rows.
#[test]
fn a_newline_inside_a_cell_is_not_a_row_boundary() {
    let f = std::path::Path::new("testdata/xl_cell_newline.xlsx");
    let spec = tdy::sniff::sniff(f, &Default::default(), Default::default()).unwrap();
    let t = tdy::engine::execute(&spec, f, Default::default()).unwrap();
    assert_eq!(t.num_rows(), 3, "an embedded newline split a row");
}

/// Sibling currency columns must all be decimal; float64 leaks IEEE noise.
#[test]
fn sibling_money_columns_all_become_decimal() {
    let f = std::path::Path::new("testdata/xl_money_siblings.xlsx");
    let spec = tdy::sniff::sniff(f, &Default::default(), Default::default()).unwrap();
    let money: Vec<_> = spec.columns.iter().filter(|c| c.name.starts_with("amount")).collect();
    assert!(money.len() >= 3);
    for c in money {
        assert!(matches!(c.dtype, tdy::spec::DType::Decimal { .. }),
                "{} typed {:?}, sibling money columns must agree", c.name, c.dtype);
    }
}
```

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.** (a) Find where a cell's rendered text is split — the row
builder must treat a cell's contents as one value regardless of embedded `\n` (replace or
retain the newline inside the field, never end the row on it). (b) Where Excel column
types are chosen, derive "this is money" from the cell **number format** (a currency format
string) rather than from the values alone, and apply it to every column whose format says
currency, so siblings cannot disagree.

- [ ] **Step 4: Run the tests**, then the whole suite. `tests/fixtures.rs` pins exact values
for `umsatz.xlsx` and friends — if a pinned money dtype changes, that is a real behaviour
change: check whether the new type is the correct one before touching the fixture.

- [ ] **Step 5: Commit**

```bash
git add src/engine.rs src/sniff.rs tests/regression.rs
git commit -m "a newline inside a cell stays inside it; money agrees with its siblings"
```

---

### Task 7: Fixtures for every defect, and the audit's own regression corpus

**Files:**
- Create: `testdata/gen/13_audit_defects.py`
- Modify: `gen_fixtures.py` (register generator 13), `CLAUDE.md` (fixtures section)

**Interfaces:** produces `testdata/torn_tail_utf8.csv`, `xl_cell_newline.xlsx`,
`xl_money_siblings.xlsx` (+ any fixture Tasks 1–6 reference by name).

- [ ] **Step 1: Write the generator.** Follow `testdata/gen/`'s conventions: a module
docstring saying what each file stresses, deterministic output, and for xlsx **pin entry
timestamps and patch `dcterms:modified`** (see CLAUDE.md — getting this wrong stales every
sidecar hash). Files to emit:
  - `torn_tail_utf8.csv` — valid UTF-8, ≥ 40 KB so head+tail sampling applies, with a
    multi-byte character positioned so the tail sample begins mid-sequence. Verify while
    generating: assert the whole file decodes and that `bytes[-4096:]` does NOT.
  - `xl_cell_newline.xlsx` — 3 data rows, one cell containing `"Manufacture\nOf Beer"`.
  - `xl_money_siblings.xlsx` — 4 columns with `"$"#,##0.00` number formats and values
    whose float64 representation is lossy (e.g. `255871181.11`, `7340146.63`).

- [ ] **Step 2: Register and generate** — `python3 gen_fixtures.py 13 --list` then
`python3 gen_fixtures.py 13`; run `python3 gen_fixtures.py` twice and confirm
`git status` is clean (byte-determinism).

- [ ] **Step 3: Run the full suite** — `cargo test --workspace --lib --tests`, plus
`cargo test --test adversarial` (it picks up new fixtures automatically and must not
regress: never panic, never hang, sniffable ⇒ queryable ⇒ reproducible).

- [ ] **Step 4: Document** — add a line to CLAUDE.md's Fixtures section naming generator 13
as the audit's regression corpus, and note in the Real data section that the 2026-09-03
audit's findings live in `gap_reports/AUDIT_FINDINGS.md` (gitignored).

- [ ] **Step 5: Commit**

```bash
git add testdata gen_fixtures.py CLAUDE.md
git commit -m "fixtures for every defect the corpus audit found"
```

---

### Task 8: Re-run the audit sample and record what moved

**Files:**
- Modify: `gap_reports/AUDIT_FINDINGS.md` (append a post-fix section)

- [ ] **Step 1: Re-sniff the 23 misread files** listed in `gap_reports/verdicts_all.py`
against the fixed binary (`cargo build --release`, then `tdy sniff <file> --no-llm` for
each; delete the sidecars afterwards). Record for each: does the defect persist, is it now
flagged (confidence < 0.8 and/or a note), or is it fixed outright.

- [ ] **Step 2: Re-run the corpus sweep** — `TDY_CORPUS=corpus cargo test --release --test
corpus -- --nocapture > gap_reports/corpus_survey_after.out 2>&1` — and diff the
confident/unsure/declined split against the pre-fix run (39% / 51% / 10%). An honest
expectation: *more* files flagged unsure, not fewer. That is the fixes working.

- [ ] **Step 3: Append the results** to `gap_reports/AUDIT_FINDINGS.md` under
"After the fixes (2026-09-XX)": the per-defect status table, the new sweep split, and any
defect still unflagged — which would be a bug that survived its own regression test and
must be re-opened rather than written off.

- [ ] **Step 4: Commit**

```bash
git add gap_reports 2>/dev/null || true
git commit --allow-empty -m "audit re-run: what the fixes moved"
```

*(`gap_reports/` is gitignored; this commit exists to mark the checkpoint. If nothing is
staged, the empty commit is the record.)*

---

## Self-Review Notes

- **Coverage:** all 8 unflagged defects are addressed — encoding (T1), header-promote ×3
  (T2), footer over-fire (T3), probe-window (T4), footnote-block at 0.80 (T5), money
  typing (T6). The 15 flagged defects are covered by T5 (stacked/repeated headers,
  footnote blocks) and T6 (cell newline); the remaining flagged ones (scale rounding,
  marker rows) already warn and are deliberately left, since the tool told the truth.
- **Deliberately not attempted:** reconstructing stacked/merged headers, and multi-table
  sheets. T5 makes those loud rather than silent, which is what the governing rule asks.
- **Type consistency:** `footer_rows` gains a `sampled: &[Vec<String>]` parameter (T3) —
  its three existing unit tests must be updated in the same task, or the build breaks.
- **Risk:** T2 and T3 change behaviour on files the current fixtures pin. Any fixture
  breakage is a finding, not a nuisance: check which behaviour is correct before editing.
