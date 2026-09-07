# Workbook Members Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A workbook whose sheets fit a target contributes one dataset member per fitting sheet, named `book.xlsx#Sheet`, discovered by `tdy fit` and locked, with one sidecar per sheet.

**Architecture:** A member identity becomes `(path, sheet: Option<String>)` — two fields in the lock and the sidecar fingerprint, a textual `path#sheet` only where people read or type it, resolved against existing members rather than split by rule. The fit layer gains `discover_sheets` (which sheets pass the cheap gates) and `fit_sheet` (fit one named sheet fully); `fit_pile` expands a multi-fitting workbook into sheet members and everything downstream (`dataset()`, the report, the console's `.accept`, the workbench preview) reads the sheet from its field.

**Tech Stack:** Rust ≥ 1.88, serde/toml for the lock and sidecars, the existing `sniff::frame_excel_sheet` and `fit_framed` machinery. No new dependencies.

**Spec:** `docs/design/2026-09-07-workbook-members.md` (decisions), building on `docs/design/2026-09-06-members-and-regions.md` (the question).

## Global Constraints

- `cargo test --workspace --lib --tests` must stay green after every task; `cargo clippy --all-targets -- -D warnings` runs in CI and is not installed locally — keep code warning-free (no unused imports, no manual char comparisons).
- **tdy never silently produces a wrong value.** A sheet member is read from the lock; `dataset()` never lists a workbook's sheets itself.
- A plain member (a file, or a workbook where exactly one sheet fits) keeps `sheet = None` and its existing sidecar name `<file>.tdy.toml`; existing locks and fixtures keep their meaning.
- A sheet member's sidecar is `<file>#<sheet>.tdy.toml`; its `SourceFingerprint.sheet` names the sheet.
- Drift is per file: one `Changed` per file, never one per sheet member.
- `fit()` (single file) keeps returning `FitError::AmbiguousFrame` for several fitting sheets; `tests/fit.rs::two_fitting_sheets_are_refused_not_ranked` must keep passing.
- Commit messages end with the session's `Co-Authored-By:` and `Claude-Session:` trailers as every commit in this repo does.
- Fixture facts: `testdata/sheet_frames_two_fit.xlsx` has sheets `Q1` (3 rows, sum 600.00) and `Q2` (3 rows, sum 1500.00), header `Datum;Region;Betrag`, dates `28.MM.2025`; `testdata/sheet_frames_one_fits.xlsx` has sheets `Hinweise`, `Daten` (4 rows, sum 1090.00, a title row above the header), `Legende`.

---

## File map

| File | Responsibility in this plan |
|---|---|
| `src/member.rs` (new) | `MemberRef { path, sheet }`: textual form and resolution against existing members. Pure. |
| `src/lib.rs` | `pub mod member;` |
| `src/spec.rs:43-49` | `SourceFingerprint.sheet: Option<String>` |
| `src/sidecar.rs` | `sidecar_path_for`, `load_member`, `save_member` — the optional-sheet forms; the old ones become the `None` case |
| `src/lockfile.rs` | `Member.sheet`, `Lock::member(path, sheet)`, drift grouped by file, `spec_digest_for` |
| `src/dataset.rs:130-178` | load each member's sidecar by (path, sheet); `rel` is the textual name |
| `src/fit.rs` | `SheetDiscovery`, `discover_sheets`, `fit_sheet`; `sheet_candidates` extracted from `fit()` |
| `src/report.rs` | `MemberReport.sheet`, `MemberReport::name()`, expansion in `fit_pile`, `--accept`/`exclude` by reference, text renderer prints names |
| `src/console/mod.rs:761-800` | `.accept` resolves a textual member against the lock |
| `tdy-tui/src/workbench.rs:1244-1256, 1515` | member preview passes the member's sheet |
| `tests/member.rs` (new), `tests/dataset.rs`, `tests/fit.rs`, `tests/json.rs`, `tdy-tui/tests/workbench.rs` | the tests named per task |
| `CLAUDE.md`, `README.md`, `docs/design/2026-09-06-members-and-regions.md` | docs |

---

### Task 1: `MemberRef` — the identity, its textual form, and resolution

**Files:**
- Create: `src/member.rs`
- Modify: `src/lib.rs` (add `pub mod member;` beside the other modules)
- Test: unit tests inside `src/member.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct MemberRef { pub path: String, pub sheet: Option<String> }
  impl MemberRef {
      pub fn file(path: impl Into<String>) -> MemberRef
      pub fn sheet(path: impl Into<String>, sheet: impl Into<String>) -> MemberRef
      pub fn name(&self) -> String            // "path" or "path#sheet"
      pub fn resolve(text: &str, exists: impl Fn(&MemberRef) -> bool) -> Option<MemberRef>
  }
  ```

- [ ] **Step 1: Write the failing tests**

```rust
// src/member.rs
//! A dataset member: a file, or one sheet of a workbook.
//!
//! Two fields, never one string: a `#` in a file name or a sheet name must
//! not make a lock ambiguous. The textual `path#sheet` exists where a person
//! reads or types a member, and a typed reference is resolved against the
//! members that exist rather than split by a rule.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemberRef {
    /// Relative to the target's directory, as lock members are.
    pub path: String,
    pub sheet: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_member_names_its_path_and_a_sheet_member_appends_the_sheet() {
        assert_eq!(MemberRef::file("2025.xlsx").name(), "2025.xlsx");
        assert_eq!(MemberRef::sheet("2025.xlsx", "Q1").name(), "2025.xlsx#Q1");
    }

    /// `a#b#c` could be file `a#b` sheet `c`, or file `a` sheet `b#c`, or a
    /// file called `a#b#c`. Whichever exists is the answer; nothing splits
    /// the string by rule.
    #[test]
    fn a_typed_reference_resolves_against_what_exists() {
        let members = vec![
            MemberRef::sheet("2025#final.xlsx", "Q1"),
            MemberRef::sheet("2025.xlsx", "Q#2"),
            MemberRef::file("plain#name.csv"),
        ];
        let exists = |m: &MemberRef| members.contains(m);
        assert_eq!(MemberRef::resolve("2025#final.xlsx#Q1", exists), Some(members[0].clone()));
        assert_eq!(MemberRef::resolve("2025.xlsx#Q#2", exists), Some(members[1].clone()));
        assert_eq!(MemberRef::resolve("plain#name.csv", exists), Some(members[2].clone()));
        assert_eq!(MemberRef::resolve("2025.xlsx#Q3", exists), None);
        assert_eq!(MemberRef::resolve("nothing.csv", exists), None);
    }
}
```

- [ ] **Step 2: Add the module and run the tests to see them fail**

Add `pub mod member;` to `src/lib.rs` next to `pub mod lockfile;`.

Run: `cargo test --lib member::`
Expected: compile error — `file`, `sheet`, `name`, `resolve` not found.

- [ ] **Step 3: Implement**

```rust
impl MemberRef {
    pub fn file(path: impl Into<String>) -> MemberRef {
        MemberRef { path: path.into(), sheet: None }
    }

    pub fn sheet(path: impl Into<String>, sheet: impl Into<String>) -> MemberRef {
        MemberRef { path: path.into(), sheet: Some(sheet.into()) }
    }

    /// The form a person reads and types: `path`, or `path#sheet`.
    pub fn name(&self) -> String {
        match &self.sheet {
            Some(s) => format!("{}#{s}", self.path),
            None => self.path.clone(),
        }
    }

    /// Resolve a typed reference against the members that exist. Every
    /// split at a `#` is a candidate — the whole text as a plain member,
    /// then each `path#sheet` split from the right — and the first that
    /// `exists` is the answer. `None` names no member.
    pub fn resolve(text: &str, exists: impl Fn(&MemberRef) -> bool) -> Option<MemberRef> {
        let plain = MemberRef::file(text);
        if exists(&plain) {
            return Some(plain);
        }
        for (i, _) in text.rmatch_indices('#') {
            let candidate = MemberRef::sheet(&text[..i], &text[i + 1..]);
            if exists(&candidate) {
                return Some(candidate);
            }
        }
        None
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib member::`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add src/member.rs src/lib.rs
git commit -m "A member is a path and an optional sheet, resolved against what exists"
```

---

### Task 2: Sidecars per (file, sheet)

**Files:**
- Modify: `src/spec.rs:43-49` (`SourceFingerprint`)
- Modify: `src/sidecar.rs:27-34` (`sidecar_path`), `:56-87` (`load`), `:97-127` (`save`)
- Test: unit tests appended to `src/sidecar.rs` (it has a `#[cfg(test)] mod tests` — if not, add one with `tempfile`)

**Interfaces:**
- Produces:
  ```rust
  pub fn sidecar_path_for(file: &Path, sheet: Option<&str>) -> PathBuf   // <file>.tdy.toml or <file>#<sheet>.tdy.toml
  pub fn load_member(file: &Path, sheet: Option<&str>) -> Result<SidecarStatus>
  pub fn save_member(file: &Path, sheet: Option<&str>, spec: &ParseSpec, prov: ProvenanceInfo) -> Result<PathBuf>
  // unchanged, now thin wrappers with sheet = None:
  pub fn sidecar_path(file: &Path) -> PathBuf
  pub fn load(file: &Path) -> Result<SidecarStatus>
  pub fn save(file: &Path, spec: &ParseSpec, prov: ProvenanceInfo) -> Result<PathBuf>
  ```
- `SourceFingerprint` gains `pub sheet: Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]` (the struct is `deny_unknown_fields`, so old sidecars still parse — the field is optional).

- [ ] **Step 1: Write the failing tests**

```rust
// appended to src/sidecar.rs, inside `mod tests` (create the module if absent):
use crate::spec::{ColumnSpec, DType, Extraction, ParseSpec, Transform, ValueParsing};

fn sheet_spec(sheet: &str) -> ParseSpec {
    ParseSpec {
        extraction: Extraction::Excel { sheet_name: Some(sheet.into()), sheet_index: None, range: None },
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![ColumnSpec {
            name: "region".into(),
            source: Some("Region".into()),
            dtype: DType::Utf8,
            nullable: false,
            parse: ValueParsing::default(),
            pointer: None,
        }],
        confidence: Some(1.0),
        notes: vec![],
    }
}

#[test]
fn a_sheet_sidecar_sits_beside_its_workbook_under_the_sheets_name() {
    let f = Path::new("/data/2025.xlsx");
    assert_eq!(sidecar_path_for(f, None), PathBuf::from("/data/2025.xlsx.tdy.toml"));
    assert_eq!(sidecar_path_for(f, Some("Q1")), PathBuf::from("/data/2025.xlsx#Q1.tdy.toml"));
    assert_eq!(sidecar_path(f), sidecar_path_for(f, None));
}

#[test]
fn a_sheet_sidecar_round_trips_and_states_its_sheet() {
    let d = tempfile::TempDir::new().unwrap();
    let book = d.path().join("book.xlsx");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_two_fit.xlsx"),
        &book,
    )
    .unwrap();
    let prov = || ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None };
    let p = save_member(&book, Some("Q1"), &sheet_spec("Q1"), prov()).unwrap();
    assert!(p.ends_with("book.xlsx#Q1.tdy.toml"), "{}", p.display());
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains("sheet = \"Q1\""), "the fingerprint names the sheet:\n{text}");

    match load_member(&book, Some("Q1")).unwrap() {
        SidecarStatus::Fresh(sc) => assert_eq!(sc.source.sheet.as_deref(), Some("Q1")),
        other => panic!("expected Fresh, got {other:?}"),
    }
    // The plain sidecar is a different file, and absent.
    assert!(matches!(load(&book).unwrap(), SidecarStatus::Absent));
    // A second sheet's sidecar is another file again.
    save_member(&book, Some("Q2"), &sheet_spec("Q2"), prov()).unwrap();
    assert!(matches!(load_member(&book, Some("Q2")).unwrap(), SidecarStatus::Fresh(_)));
    assert!(matches!(load_member(&book, Some("Q1")).unwrap(), SidecarStatus::Fresh(_)));
}
```

`SidecarStatus` must derive `Debug` for the `panic!` above; add `#[derive(Debug)]` to it if it lacks one.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --lib sidecar::tests`
Expected: compile error — `sidecar_path_for`, `save_member`, `load_member` not found.

- [ ] **Step 3: Implement**

In `src/spec.rs`, `SourceFingerprint`:

```rust
pub struct SourceFingerprint {
    /// Path relative to the sidecar's location (survives repo relocation).
    pub path: String,
    /// blake3 of the full file; mismatch at query time = stale spec.
    pub blake3: String,
    pub bytes: u64,
    /// The sheet this spec is about, for a sheet member's sidecar
    /// (`<file>#<sheet>.tdy.toml`). Absent for a plain sidecar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
}
```

Every other constructor of `SourceFingerprint` in the tree gains `sheet: None` — `grep -rn "SourceFingerprint {" src tests tdy-tui` and add the field to each.

In `src/sidecar.rs`:

```rust
/// `<file>.tdy.toml`, or `<file>#<sheet>.tdy.toml` for one sheet of a
/// workbook: the selector is part of the sidecar's *name*, so validate,
/// --stamp, `.edit` and the browser's companion folding need nothing.
pub fn sidecar_path_for(file: &Path, sheet: Option<&str>) -> PathBuf {
    let mut name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(s) = sheet {
        name.push('#');
        name.push_str(s);
    }
    name.push_str(".tdy.toml");
    file.with_file_name(name)
}

pub fn sidecar_path(file: &Path) -> PathBuf {
    sidecar_path_for(file, None)
}

pub fn load(file: &Path) -> Result<SidecarStatus> {
    load_member(file, None)
}

pub fn load_member(file: &Path, sheet: Option<&str>) -> Result<SidecarStatus> {
    let sc_path = sidecar_path_for(file, sheet);
    // ... the existing body of `load`, unchanged, using `sc_path` ...
}

pub fn save(file: &Path, spec: &ParseSpec, prov: ProvenanceInfo) -> Result<PathBuf> {
    save_member(file, None, spec, prov)
}

pub fn save_member(file: &Path, sheet: Option<&str>, spec: &ParseSpec, prov: ProvenanceInfo) -> Result<PathBuf> {
    let (hash, bytes) = hash_file(file)?;
    let sidecar = Sidecar {
        spec_version: SPEC_FORMAT_VERSION,
        source: SourceFingerprint {
            path: /* as before */,
            blake3: hash,
            bytes,
            sheet: sheet.map(str::to_string),
        },
        provenance: /* as before */,
        spec: spec.clone(),
    };
    let sc_path = sidecar_path_for(file, sheet);
    // ... the existing serialise + atomic_write ...
}
```

`stamp(file, method)` is left on the plain sidecar; a sheet sidecar is written by `fit`, never hand-stamped in this slice.

- [ ] **Step 4: Run the tests and the whole suite**

Run: `cargo test --lib sidecar::tests` then `cargo test --workspace --lib --tests`
Expected: all green (the new `sheet` field is optional everywhere).

- [ ] **Step 5: Commit**

```bash
git add src/spec.rs src/sidecar.rs
git commit -m "One sidecar per sheet: <file>#<sheet>.tdy.toml, its fingerprint naming the sheet"
```

---

### Task 3: The lock carries a sheet; drift stays per file

**Files:**
- Modify: `src/lockfile.rs:29-57` (`Member`), `:175-177` (`Lock::member`), `:201-207` (`spec_digest`), `:239-300` (`drift`)
- Modify (constructors gaining `sheet: None`): `src/report.rs` (two `Member {` literals, ~lines 486 and 552), `tdy-tui/src/workbench.rs` (one), `tests/mcp.rs` (one)
- Test: unit tests appended to `src/lockfile.rs`'s `mod tests`

**Interfaces:**
- Produces:
  ```rust
  pub struct Member { pub path: String, pub sheet: Option<String>, /* existing fields */ }
  impl Member { pub fn name(&self) -> String }          // via MemberRef
  impl Lock { pub fn member(&self, path: &str, sheet: Option<&str>) -> Option<&Member> }
  pub fn spec_digest_for(data_file: &Path, sheet: Option<&str>) -> String
  pub fn spec_digest(data_file: &Path) -> String        // = spec_digest_for(.., None)
  ```
- `Drift::Duplicated(String)` carries the member's `name()`; `Drift::Changed/Removed(String)` carry the file path (once per file).

- [ ] **Step 1: Write the failing tests**

```rust
// appended to src/lockfile.rs `mod tests`
use crate::member::MemberRef;

fn m(path: &str, sheet: Option<&str>) -> Member {
    Member {
        path: path.into(),
        sheet: sheet.map(str::to_string),
        blake3: "b3:x".into(),
        bytes: 1,
        spec_digest: String::new(),
        review: None,
        accepted: false,
    }
}

#[test]
fn a_sheet_member_round_trips_through_toml_and_a_plain_one_writes_no_sheet() {
    let lock = Lock {
        lock_version: LOCK_VERSION,
        target: "t".into(),
        target_hash: "b3:t".into(),
        tool_version: "0".into(),
        created_at: "now".into(),
        members: vec![m("a.xlsx", Some("Q1")), m("b.csv", None)],
    };
    let text = toml::to_string_pretty(&lock).unwrap();
    assert!(text.contains("sheet = \"Q1\""), "{text}");
    assert_eq!(text.matches("sheet =").count(), 1, "a plain member writes no sheet:\n{text}");
    let back: Lock = toml::from_str(&text).unwrap();
    assert_eq!(back.members[0].sheet.as_deref(), Some("Q1"));
    assert_eq!(back.members[0].name(), "a.xlsx#Q1");
    assert_eq!(back.members[1].name(), "b.csv");
    assert!(back.member("a.xlsx", Some("Q1")).is_some());
    assert!(back.member("a.xlsx", None).is_none(), "the plain member of that file does not exist");
}

/// Two sheets of one workbook are two members; the same sheet twice is a
/// duplicate. And a changed workbook is one `Changed`, not one per sheet.
#[test]
fn drift_is_per_file_and_duplication_is_per_sheet() {
    let d = tempfile::TempDir::new().unwrap();
    let book = d.path().join("book.xlsx");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_two_fit.xlsx"),
        &book,
    )
    .unwrap();
    let t = d.path().join("t.tdy.sql");
    std::fs::write(&t, "CREATE TABLE t (region TEXT) WITH (files = '*.xlsx');").unwrap();
    let target = crate::target::Target::load(&t).unwrap();
    let (hash, bytes) = crate::sidecar::hash_file(&book).unwrap();
    let fresh = |sheet: &str| Member { blake3: hash.clone(), bytes, ..m("book.xlsx", Some(sheet)) };
    let mut lock = Lock {
        lock_version: LOCK_VERSION,
        target: "t".into(),
        target_hash: target_hash(&target),
        tool_version: "0".into(),
        created_at: "now".into(),
        members: vec![fresh("Q1"), fresh("Q2")],
    };
    assert!(drift(&lock, &target, &t).unwrap().is_empty(), "two sheets of one file are two members");

    lock.members.push(fresh("Q1"));
    let d1 = drift(&lock, &target, &t).unwrap();
    assert!(matches!(&d1[..], [Drift::Duplicated(n)] if n == "book.xlsx#Q1"), "{d1:?}");
    lock.members.pop();

    // Touch the workbook: every sheet member's proof is void, said once.
    let mut bytes_on_disk = std::fs::read(&book).unwrap();
    bytes_on_disk.push(0);
    std::fs::write(&book, bytes_on_disk).unwrap();
    let d2 = drift(&lock, &target, &t).unwrap();
    assert_eq!(d2, vec![Drift::Changed("book.xlsx".into())], "{d2:?}");
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --lib lockfile::tests`
Expected: compile error — `Member` has no field `sheet`; `member` takes one argument.

- [ ] **Step 3: Implement**

```rust
// src/lockfile.rs — Member
pub struct Member {
    pub path: String,
    /// One sheet of a workbook, when a workbook contributes several
    /// members; absent for a plain member. Two fields rather than a
    /// `path#sheet` string, so a `#` in either name cannot make the lock
    /// ambiguous (`crate::member::MemberRef`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    pub blake3: String,
    // ... existing fields unchanged ...
}

impl Member {
    /// The form a person reads and types.
    pub fn name(&self) -> String {
        MemberRef { path: self.path.clone(), sheet: self.sheet.clone() }.name()
    }
}

impl Lock {
    pub fn member(&self, path: &str, sheet: Option<&str>) -> Option<&Member> {
        self.members.iter().find(|m| m.path == path && m.sheet.as_deref() == sheet)
    }
}

pub fn spec_digest(data_file: &Path) -> String {
    spec_digest_for(data_file, None)
}

pub fn spec_digest_for(data_file: &Path, sheet: Option<&str>) -> String {
    let p = crate::sidecar::sidecar_path_for(data_file, sheet);
    match std::fs::read(&p) {
        Ok(bytes) => format!("b3:{}", blake3::hash(&bytes).to_hex()),
        Err(_) => String::new(),
    }
}
```

`drift`, replacing the body from the duplicate check to the end:

```rust
    let locked_files: BTreeSet<&str> = lock.members.iter().map(|m| m.path.as_str()).collect();

    // Two sheets of one workbook are two members; the same (path, sheet)
    // twice would be read twice.
    {
        let mut seen: BTreeSet<(&str, Option<&str>)> = BTreeSet::new();
        for m in &lock.members {
            if !seen.insert((m.path.as_str(), m.sheet.as_deref())) {
                out.push(Drift::Duplicated(m.name()));
            }
        }
    }

    for rel in &on_disk {
        if !locked_files.contains(rel.as_str()) {
            out.push(Drift::Added(rel.clone()));
        }
    }

    // Per file: the hash covers every sheet, so a renamed, added or edited
    // sheet is one `Changed` on the file, and the refit rediscovers the
    // sheet set. Members are grouped by path, in lock order.
    let mut by_file: Vec<(&str, Vec<&Member>)> = Vec::new();
    for m in &lock.members {
        match by_file.iter_mut().find(|(p, _)| *p == m.path.as_str()) {
            Some((_, ms)) => ms.push(m),
            None => by_file.push((m.path.as_str(), vec![m])),
        }
    }
    for (path, members) in by_file {
        let p = dir.join(path);
        if !p.exists() {
            out.push(Drift::Removed(path.to_string()));
            continue;
        }
        let (hash, bytes) = crate::sidecar::hash_file(&p)?;
        if members.iter().any(|m| hash != m.blake3 || bytes != m.bytes) {
            out.push(Drift::Changed(path.to_string()));
            continue;
        }
        for m in members {
            if m.accepted
                && !m.spec_digest.is_empty()
                && spec_digest_for(&p, m.sheet.as_deref()) != m.spec_digest
            {
                out.push(Drift::SpecEdited(m.name()));
            }
        }
    }
    Ok(out)
```

`Drift` needs `PartialEq` for the `assert_eq!` — it already derives it (`#[derive(Debug, Clone, PartialEq)]`).

Then the constructor sites: add `sheet: None,` to the two `Member {` literals in `src/report.rs`, the one in `tdy-tui/src/workbench.rs`, and the one in `tests/mcp.rs`; change the two `l.member(rel)` calls in `src/report.rs` to `l.member(rel, None)` (Task 5 revisits them).

- [ ] **Step 4: Run the tests and the whole suite**

Run: `cargo test --lib lockfile::tests` then `cargo test --workspace --lib --tests`
Expected: green. `tests/dataset.rs::a_member_listed_twice_is_drift` still passes (its message still contains "twice").

- [ ] **Step 5: Commit**

```bash
git add src/lockfile.rs src/report.rs tdy-tui/src/workbench.rs tests/mcp.rs
git commit -m "A lock member carries its sheet; drift stays per file, duplication per sheet"
```

---

### Task 4: `dataset()` reads sheet members — the hand-written-lock proof

**Files:**
- Modify: `src/dataset.rs:132-176`
- Test: `tests/dataset.rs` (append)

**Interfaces:**
- Consumes: `sidecar::load_member`, `Member.sheet`, `Member::name()`.
- Produces: `ResolvedMember.rel` is the member's `name()` (so `_member` shows `book.xlsx#Q1`).

- [ ] **Step 1: Write the failing test**

```rust
// tests/dataset.rs (append). Uses the file's existing `tdy()` helper.
use tdy::lockfile::{Lock, Member, LOCK_VERSION};
use tdy::spec::{ColumnSpec, DType, Extraction, InferenceMethod, ParseSpec, Transform, ValueParsing};

fn quarter_spec(sheet: &str) -> ParseSpec {
    let col = |name: &str, source: &str, dtype: DType| ColumnSpec {
        name: name.into(),
        source: Some(source.into()),
        dtype,
        nullable: false,
        parse: ValueParsing::default(),
        pointer: None,
    };
    ParseSpec {
        extraction: Extraction::Excel { sheet_name: Some(sheet.into()), sheet_index: None, range: None },
        transforms: vec![Transform::PromoteHeader { rows: 1, join: " ".into() }],
        columns: vec![
            col("month", "Datum", DType::Date { format: "%d.%m.%Y".into() }),
            col("region", "Region", DType::Utf8),
            col("amount", "Betrag", DType::Decimal { precision: 14, scale: 2 }),
        ],
        confidence: Some(1.0),
        notes: vec![],
    }
}

/// Step one of the design: a lock naming two sheets of one workbook, written
/// by hand with hand-written sheet sidecars, is read by `dataset()` as two
/// members — the sum of both sheets, each row saying which sheet it came from.
/// No discovery is involved yet; this proves the identity model alone.
#[test]
fn a_hand_written_lock_over_two_sheets_reads_both_and_names_them() {
    let dir = TempDir::new().unwrap();
    let book = dir.path().join("2025.xlsx");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_two_fit.xlsx"),
        &book,
    )
    .unwrap();
    let t = dir.path().join("monat.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE monat (\n  month DATE NOT NULL, region TEXT NOT NULL, amount DECIMAL(14,2) NOT NULL\n) \
         WITH (files = '*.xlsx', date_order = 'dmy', provenance = 'true');\n",
    )
    .unwrap();
    let target = tdy::target::Target::load(&t).unwrap();
    let prov = || tdy::sidecar::ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None };
    for sheet in ["Q1", "Q2"] {
        tdy::sidecar::save_member(&book, Some(sheet), &quarter_spec(sheet), prov()).unwrap();
    }
    let (blake3, bytes) = tdy::sidecar::hash_file(&book).unwrap();
    let member = |sheet: &str| Member {
        path: "2025.xlsx".into(),
        sheet: Some(sheet.into()),
        blake3: blake3.clone(),
        bytes,
        spec_digest: tdy::lockfile::spec_digest_for(&book, Some(sheet)),
        review: None,
        accepted: false,
    };
    Lock {
        lock_version: LOCK_VERSION,
        target: "monat".into(),
        target_hash: tdy::lockfile::target_hash(&target),
        tool_version: "test".into(),
        created_at: "now".into(),
        members: vec![member("Q1"), member("Q2")],
    }
    .save(&t)
    .unwrap();

    let sql = format!(
        "SELECT _member, count(*) AS n, sum(amount) AS total FROM dataset('{}') GROUP BY _member ORDER BY _member",
        t.display()
    );
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("2025.xlsx#Q1") && text.contains("600.00"), "{text}");
    assert!(text.contains("2025.xlsx#Q2") && text.contains("1500.00"), "{text}");
    assert_eq!(text.matches("| 3 ").count(), 2, "three rows per sheet:\n{text}");
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --test dataset a_hand_written_lock_over_two_sheets`
Expected: FAIL — `dataset()` looks for `2025.xlsx.tdy.toml` (absent) and errors "is a member of `monat` but has no spec".

- [ ] **Step 3: Implement**

In `src/dataset.rs`'s member loop:

```rust
        let spec = match crate::sidecar::load_member(&path, m.sheet.as_deref())? {
            crate::sidecar::SidecarStatus::Fresh(sc) => sc.spec,
            crate::sidecar::SidecarStatus::Stale(_) => anyhow::bail!(
                "{} has changed since it was fitted — run `tdy fit {}`",
                m.name(),
                target_file.display()
            ),
            crate::sidecar::SidecarStatus::Absent => anyhow::bail!(
                "{} is a member of `{}` but has no spec — run `tdy fit {}`",
                m.name(),
                target.name,
                target_file.display()
            ),
        };
        // ... conformance check, using m.name() in its message ...
        members.push(ResolvedMember {
            path,
            rel: m.name(),
            spec: Arc::new(spec),
        });
```

Also the "waiting on a human" listing above it (`for m in &unreviewed`) prints `m.name()` instead of `m.path`.

- [ ] **Step 4: Run the test and the suite**

Run: `cargo test --test dataset` then `cargo test --workspace --lib --tests`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add src/dataset.rs tests/dataset.rs
git commit -m "dataset() reads sheet members from the lock, proved by a hand-written one"
```

---

### Task 5: `fit::discover_sheets` and `fit::fit_sheet`

**Files:**
- Modify: `src/fit.rs:442-535` (`fit()`), add the two functions and `sheet_candidates` beside `fit_by_elimination`
- Test: `tests/fit.rs` (append)

**Interfaces:**
- Produces:
  ```rust
  pub struct SheetDiscovery { pub total: usize, pub fitting: Vec<String>, pub rejected: Vec<String> }
  /// `None` for a non-workbook or a one-sheet workbook. Order is the workbook's.
  pub fn discover_sheets(path: &Path, target: &Target, limits: Limits) -> Result<Option<SheetDiscovery>, FitError>
  /// Fit one named sheet, fully (Rigour::Full), with the elimination note.
  pub fn fit_sheet(path: &Path, sheet: &str, target: &Target, limits: Limits) -> Result<Fitted, FitError>
  ```

- [ ] **Step 1: Write the failing tests**

```rust
// tests/fit.rs (append). `SHEET_TARGET` and `frames_fixture` already exist in this file.
use tdy::fit::{discover_sheets, fit_sheet};

/// Discovery says which sheets pass the gates, in the workbook's order, and
/// names the ones that do not — without choosing.
#[test]
fn discovery_names_the_fitting_sheets_and_the_rejected_ones() {
    let t = Target::parse(SHEET_TARGET).unwrap();
    let two = discover_sheets(&frames_fixture("sheet_frames_two_fit.xlsx"), &t, Limits::default())
        .unwrap()
        .expect("a two-sheet workbook is discoverable");
    assert_eq!(two.total, 2);
    assert_eq!(two.fitting, vec!["Q1".to_string(), "Q2".to_string()]);
    assert!(two.rejected.is_empty());

    let one = discover_sheets(&frames_fixture("sheet_frames_one_fits.xlsx"), &t, Limits::default())
        .unwrap()
        .expect("three sheets");
    assert_eq!(one.total, 3);
    assert_eq!(one.fitting, vec!["Daten".to_string()]);
    assert_eq!(one.rejected, vec!["Hinweise".to_string(), "Legende".to_string()]);

    let csv = corpus().join("2025-01.csv");
    assert!(discover_sheets(&csv, &target(), Limits::default()).unwrap().is_none(), "not a workbook");
}

/// One named sheet, fully fitted: its own frame, its own sum.
#[test]
fn a_named_sheet_is_fitted_on_its_own() {
    let t = Target::parse(SHEET_TARGET).unwrap();
    let p = frames_fixture("sheet_frames_two_fit.xlsx");
    let q2 = fit_sheet(&p, "Q2", &t, Limits::default()).expect("Q2 fits");
    assert!(matches!(&q2.spec.extraction, tdy::spec::Extraction::Excel { sheet_name: Some(s), .. } if s == "Q2"));
    let batches = tdy::engine::execute_batches(&q2.spec, &p, Limits::default()).unwrap();
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 3);
    let total: i128 = batches
        .iter()
        .flat_map(|b| {
            let a = b.column_by_name("amount").unwrap();
            let a = a.as_any().downcast_ref::<datafusion::arrow::array::Decimal128Array>().unwrap();
            (0..a.len()).map(|i| a.value(i)).collect::<Vec<_>>()
        })
        .sum();
    assert_eq!(total, 150000, "sum(amount) of Q2 is 1500.00");
    assert!(fit_sheet(&p, "Q9", &t, Limits::default()).is_err(), "a sheet that does not exist");
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --test fit discovery_names -- --nocapture; cargo test --test fit a_named_sheet`
Expected: compile error — `discover_sheets`, `fit_sheet` not found in `tdy::fit`.

- [ ] **Step 3: Implement**

Extract the workbook block out of `fit()` into a helper both callers use, then add the two public functions:

```rust
/// A workbook's sheets, each framed on its own: the sniffer's pick first
/// (so a failure message is about the sheet the user would have been
/// shown), then the rest in workbook order. `None` when the file is not a
/// workbook with several sheets.
fn sheet_candidates(
    path: &Path,
    draft: &ParseSpec,
    limits: Limits,
) -> Option<(Vec<String>, Vec<(String, ParseSpec)>)> {
    let Extraction::Excel { sheet_name, .. } = &draft.extraction else { return None };
    let shapes = crate::engine::excel_sheet_shapes(path, limits).unwrap_or_default();
    if shapes.len() <= 1 {
        return None;
    }
    let mut names: Vec<String> = Vec::with_capacity(shapes.len());
    if let Some(picked) = sheet_name {
        names.push(picked.clone());
    }
    for sh in &shapes {
        if !names.contains(&sh.name) {
            names.push(sh.name.clone());
        }
    }
    let candidates = names
        .iter()
        .filter_map(|n| sniff::frame_excel_sheet(path, n, limits).ok().map(|d| (n.clone(), d)))
        .collect();
    Some((names, candidates))
}

/// The frame the sniffer gives this file — what `fit()` starts from.
fn sniff_draft(path: &Path, target: &Target, limits: Limits) -> Result<ParseSpec, FitError> {
    let sample = crate::sample::build(path, 16 * 1024, limits)
        .with_context(|| format!("sampling {}", path.display()))
        .map_err(FitError::Unreadable)?;
    Ok(sniff::sniff_opts(path, &sample, limits, sniff::SniffOpts { verify: target.verify == Verify::Full })
        .with_context(|| format!("framing {}", path.display()))
        .map_err(FitError::Unreadable)?
        .spec)
}
```

`fit()` becomes: `let draft = sniff_draft(path, target, limits)?;` then the JSON block as before, then

```rust
    if let Some((names, candidates)) = sheet_candidates(path, &draft, limits) {
        if !candidates.is_empty() {
            return fit_by_elimination(
                path,
                target,
                limits,
                FrameCandidates { what: "sheets", field: "sheet_name", total: names.len(), candidates },
            );
        }
    }
    fit_framed(path, target, limits, draft, Rigour::Full)
```

(the JSON arm keeps its own match; only the Excel arm moves.) Then:

```rust
/// Which sheets of a workbook produce the declared table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetDiscovery {
    /// Every sheet, counting ones that could not even be framed.
    pub total: usize,
    /// In the workbook's order. Each passed the cheap gates (`Rigour::Gates`).
    pub fitting: Vec<String>,
    /// The rest, in the same order: could not be framed, or did not fit.
    pub rejected: Vec<String>,
}

/// Ask a workbook which of its sheets pass the gates against `target`,
/// without choosing between them. `None` for a file that is not a workbook
/// with several sheets — the plain-member case, which `fit`/`plan` handle.
pub fn discover_sheets(
    path: &Path,
    target: &Target,
    limits: Limits,
) -> Result<Option<SheetDiscovery>, FitError> {
    let draft = sniff_draft(path, target, limits)?;
    let Some((_, candidates)) = sheet_candidates(path, &draft, limits) else { return Ok(None) };
    let shapes = crate::engine::excel_sheet_shapes(path, limits).map_err(FitError::Unreadable)?;
    let mut fitting = Vec::new();
    let mut rejected = Vec::new();
    for sh in &shapes {
        let passes = candidates
            .iter()
            .find(|(n, _)| *n == sh.name)
            .map(|(_, d)| fit_framed(path, target, limits, d.clone(), Rigour::Gates).is_ok())
            .unwrap_or(false);
        if passes { fitting.push(sh.name.clone()) } else { rejected.push(sh.name.clone()) }
    }
    Ok(Some(SheetDiscovery { total: shapes.len(), fitting, rejected }))
}

/// Fit one named sheet of a workbook, fully.
pub fn fit_sheet(path: &Path, sheet: &str, target: &Target, limits: Limits) -> Result<Fitted, FitError> {
    let draft = sniff::frame_excel_sheet(path, sheet, limits)
        .with_context(|| format!("framing sheet {sheet:?} of {}", path.display()))
        .map_err(FitError::Unreadable)?;
    fit_framed(path, target, limits, draft, Rigour::Full)
}
```

`Rigour` must derive `Clone, Copy` if it does not; `sniff::frame_excel_sheet` is `pub(crate)`, which suffices since `fit.rs` is in the crate.

- [ ] **Step 4: Run the tests and the suite**

Run: `cargo test --test fit` then `cargo test --workspace --lib --tests`
Expected: green, including `two_fitting_sheets_are_refused_not_ranked` and `an_excel_frame_is_proved_by_elimination_when_only_one_sheet_fits`, which must be untouched by the refactor.

- [ ] **Step 5: Commit**

```bash
git add src/fit.rs tests/fit.rs
git commit -m "fit: discover which sheets fit, and fit one sheet by name"
```

---

### Task 6: `fit_pile` expands a workbook into sheet members

**Files:**
- Modify: `src/report.rs` (`MemberReport`, `fit_pile`, `render_pile_text`)
- Modify: `src/progress.rs` only if `Event::MemberStarted/Finished` print `path` — they carry a `String`, which now receives the member's name; no type change.
- Test: `tests/dataset.rs` (append), `tests/json.rs` (append)

**Interfaces:**
- Consumes: `fit::discover_sheets`, `fit::fit_sheet`, `sidecar::load_member/save_member`, `lockfile::spec_digest_for`, `Lock::member(path, sheet)`, `MemberRef`.
- Produces:
  ```rust
  pub struct MemberReport { pub path: String, pub sheet: Option<String>, /* existing */ }
  impl MemberReport { pub fn name(&self) -> String }   // path#sheet
  ```
  `MemberReport.path` stays the **file** path relative to the target (what the workbench joins to preview); `name()` is what the text renderer prints and what `--accept` takes.

- [ ] **Step 1: Write the failing tests**

```rust
// tests/dataset.rs (append)
fn quarters_pile() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_two_fit.xlsx"),
        dir.path().join("2025.xlsx"),
    )
    .unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_one_fits.xlsx"),
        dir.path().join("2024.xlsx"),
    )
    .unwrap();
    let t = dir.path().join("monat.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE monat (\n  month DATE NOT NULL OPTIONS(matches = 'Datum'),\n  region TEXT NOT NULL OPTIONS(matches = 'Region'),\n  amount DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')\n) \
         WITH (files = '*.xlsx', date_order = 'dmy', provenance = 'true');\n",
    )
    .unwrap();
    (dir, t)
}

/// Step two of the design: `tdy fit` expands the workbook whose two sheets
/// fit into two members, keeps the workbook where one sheet fits as one
/// plain member, locks all three, and the dataset is their sum.
#[test]
fn a_workbook_whose_sheets_fit_becomes_one_member_per_sheet() {
    let (dir, t) = quarters_pile();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("2025.xlsx#Q1") && text.contains("2025.xlsx#Q2"), "{text}");
    assert!(text.contains("3 of 3 file(s) fit"), "{text}");
    assert!(text.contains("of 3 sheets"), "the rejected sheets are on the record: {text}");

    let lock = std::fs::read_to_string(dir.path().join("monat.tdy.lock")).unwrap();
    assert_eq!(lock.matches("[[member]]").count(), 3, "{lock}");
    assert_eq!(lock.matches("sheet = ").count(), 2, "the plain member writes no sheet:\n{lock}");
    assert!(dir.path().join("2025.xlsx#Q1.tdy.toml").exists());
    assert!(dir.path().join("2025.xlsx#Q2.tdy.toml").exists());
    assert!(dir.path().join("2024.xlsx.tdy.toml").exists(), "one fitting sheet stays a plain member");
    assert!(!dir.path().join("2025.xlsx.tdy.toml").exists(), "an expanded workbook has no plain sidecar");

    let sql = format!("SELECT count(*), sum(amount) FROM dataset('{}')", t.display());
    let out = tdy(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("| 10 ") && text.contains("3190.00"), "600 + 1500 + 1090 over 3 + 3 + 4 rows:\n{text}");
}

/// `exclude` takes an exact member reference: one sheet goes, the other stays.
#[test]
fn a_sheet_member_can_be_excluded_by_reference() {
    let (dir, t) = quarters_pile();
    let ddl = std::fs::read_to_string(&t).unwrap().replace("files = '*.xlsx',", "files = '*.xlsx', exclude = '2025.xlsx#Q2',");
    std::fs::write(&t, ddl).unwrap();
    let out = tdy(&["fit", t.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let lock = std::fs::read_to_string(dir.path().join("monat.tdy.lock")).unwrap();
    assert!(lock.contains("sheet = \"Q1\"") && !lock.contains("sheet = \"Q2\""), "{lock}");
}

/// `--accept` names a member the way the report does; a reference that
/// names none is refused with the real names listed.
#[test]
fn accept_takes_a_member_reference_and_names_the_members_when_it_misses() {
    let (_dir, t) = quarters_pile();
    let out = tdy(&["fit", t.to_str().unwrap(), "--accept", "2025.xlsx#Q3"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("not a member") && err.contains("2025.xlsx#Q1"), "{err}");
}
```

```rust
// tests/json.rs (append; the file has helpers to run `--json fit` — reuse its `tdy` runner)
#[test]
fn json_members_carry_their_sheet() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_two_fit.xlsx"),
        dir.path().join("2025.xlsx"),
    )
    .unwrap();
    let t = dir.path().join("monat.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE monat (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files = '*.xlsx', date_order = 'dmy');",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["--json", "fit", t.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{v:#}");
    assert_eq!(members[0]["path"], "2025.xlsx");
    assert_eq!(members[0]["sheet"], "Q1");
    assert_eq!(members[1]["sheet"], "Q2");
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --test dataset a_workbook_whose_sheets_fit; cargo test --test json json_members_carry`
Expected: the first fails at "3 of 3 file(s) fit" — today the pile reports `2025.xlsx` as a GAP (AmbiguousFrame) and no lock is written; the JSON test finds one member and no `sheet`.

- [ ] **Step 3: Implement**

`MemberReport`:

```rust
pub struct MemberReport {
    /// The member's file, relative to the target.
    pub path: String,
    /// One sheet of that file, when the workbook contributed several
    /// members. `name()` is what the text shows and what `--accept` takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    // ... existing fields ...
}

impl MemberReport {
    pub fn name(&self) -> String {
        crate::member::MemberRef { path: self.path.clone(), sheet: self.sheet.clone() }.name()
    }
}
```

Every `MemberReport {` literal in `src/report.rs` (five: the reused-sidecar success, contradicts, error, planned success, planned failure) gains `sheet: unit.sheet.clone()`; the ones in `tdy-tui/tests/*.rs` and `tdy-tui/src/*.rs` gain `sheet: None` (grep `MemberReport {`).

`fit_pile`, replacing the member loop's head — the `rels` list becomes a list of units:

```rust
    use crate::member::MemberRef;

    // Expansion: a workbook whose sheets fit becomes one unit per fitting
    // sheet. Discovery runs on every fit and is never read back from the
    // previous lock — membership comes from the fit and is locked.
    let mut units: Vec<(MemberRef, Vec<String>)> = Vec::new(); // (member, notes)
    for rel in &rels {
        let p = dir.join(rel);
        match crate::fit::discover_sheets(&p, &target, limits) {
            Ok(Some(d)) if d.fitting.len() >= 2 => {
                let note = format!(
                    "of {} sheets, {} produce the declared table: {}{}",
                    d.total,
                    d.fitting.len(),
                    d.fitting.join(", "),
                    if d.rejected.is_empty() { String::new() } else { format!("; {} do not", d.rejected.join(", ")) }
                );
                for sheet in &d.fitting {
                    units.push((MemberRef::sheet(rel.clone(), sheet.clone()), vec![note.clone()]));
                }
            }
            // One fitting sheet, none, a non-workbook, or an unreadable
            // file: a plain member, and `plan` says what is wrong.
            _ => units.push((MemberRef::file(rel.clone()), Vec::new())),
        }
    }
    // `exclude` also takes exact member references, applied after expansion.
    units.retain(|(m, _)| !target.exclude.iter().any(|x| x.contains('#') && *x == m.name()));

    // `--accept` names members the way the report does.
    let accepted_now: Vec<MemberRef> = opts
        .accept
        .iter()
        .map(|a| {
            let a = a.strip_prefix(&dir).unwrap_or(a);
            let text = a.to_string_lossy().replace('\\', "/");
            MemberRef::resolve(&text, |m| units.iter().any(|(u, _)| u == m)).ok_or_else(|| {
                anyhow::anyhow!(
                    "--accept {text:?} is not a member of `{}`. Members are named relative to the \
                     target: {}",
                    target.name,
                    units.iter().take(6).map(|(u, _)| format!("{:?}", u.name())).collect::<Vec<_>>().join(", ")
                )
            })
        })
        .collect::<Result<_>>()?;

    let total = units.len();
    for (index, (unit, unit_notes)) in units.iter().enumerate() {
        let rel = &unit.path;
        let sheet = unit.sheet.as_deref();
        let name = unit.name();
        let p = dir.join(rel);
        // progress events carry `name`
        // ... existing body, with these substitutions:
        //   crate::sidecar::load(&p)                      -> crate::sidecar::load_member(&p, sheet)
        //   crate::sidecar::save(&p, ..)                  -> crate::sidecar::save_member(&p, sheet, ..)
        //   lockfile::spec_digest(&p)                     -> lockfile::spec_digest_for(&p, sheet)
        //   l.member(rel)                                 -> l.member(rel, sheet)
        //   accepted_now.iter().any(|a| a == rel)         -> accepted_now.contains(unit)
        //   crate::fit::plan(&p, ..).await                -> match sheet {
        //                                                        Some(s) => crate::fit::fit_sheet(&p, s, &target, limits)
        //                                                                      .map(|fitted| crate::fit::Planned { fitted, method: InferenceMethod::Heuristic, model: None }),
        //                                                        None => crate::fit::plan(&p, &target, cfg, opts.progress.as_ref()).await,
        //                                                    }
        //   every `MemberReport { path: rel.clone(), ..` -> `MemberReport { path: rel.clone(), sheet: unit.sheet.clone(), ..`
        //   `notes: fitted.spec.notes.clone()`           -> `notes: unit_notes.iter().cloned().chain(fitted.spec.notes.iter().cloned()).collect()`
        //   `Member { path: rel.clone(), ..`             -> `Member { path: rel.clone(), sheet: unit.sheet.clone(), ..`
    }
```

`accepted_now` must be computed after `units` (it resolves against them); move the block accordingly. `rels.is_empty()` and the confinement loop stay on `rels`.

`render_pile_text`: every `m.path` in a member line, the `Accept:` hint and the CONTRADICTS line becomes `m.name()`.

- [ ] **Step 4: Run the tests and the suite**

Run: `cargo test --test dataset; cargo test --test json; cargo test --workspace --lib --tests`
Expected: green. `tests/fit.rs::the_fitted_corpus_sums_to_the_declared_ground_truth` and the console/MCP text-equality tests must be unchanged — no plain member's text changed.

- [ ] **Step 5: Commit**

```bash
git add src/report.rs tests/dataset.rs tests/json.rs tdy-tui
git commit -m "fit expands a workbook whose sheets fit into one member per sheet"
```

---

### Task 7: The console's `.accept` and the workbench's preview know sheets

**Files:**
- Modify: `src/console/mod.rs:761-800` (`Command::Accept`)
- Modify: `tdy-tui/src/workbench.rs:1244-1256` (`enter_pile_member`), `:1515-1518` (`member_preview_path`)
- Test: `tdy-tui/tests/workbench.rs` (append), `tests/console.rs` (append)

**Interfaces:**
- Consumes: `MemberRef::resolve`, `Lock::load`, `sidecar::load_member`, `MemberReport.sheet`.

- [ ] **Step 1: Write the failing tests**

```rust
// tdy-tui/tests/workbench.rs (append). `pile_and_enter`, `member` exist in this file.
/// Entering a sheet member previews *that sheet*: the raw head asked for is
/// the member's file with the member's sheet, not the workbook's first.
#[test]
fn entering_a_sheet_member_previews_its_sheet() {
    let d = pile();
    let mut m = member("2025.xlsx", MemberStatus::Fits);
    m.sheet = Some("Q2".into());
    let (_w, act) = pile_and_enter(&d, vec![m], 0);
    match act {
        WbAction::PreviewFile { path, sheet } => {
            assert!(path.ends_with("2025.xlsx"), "{}", path.display());
            assert_eq!(sheet.as_deref(), Some("Q2"));
        }
        other => panic!("expected PreviewFile, got {other:?}"),
    }
}
```

```rust
// tests/console.rs (append). The file has a `session()`/`run` helper pattern — use the same
// construction as its existing `.accept` test (search for "pending" or ".accept" in the file).
/// `.accept T book.xlsx#Q1` resolves the reference against the lock, so
/// step one finds the sheet member's sidecar rather than a plain one.
#[tokio::test]
async fn accept_resolves_a_sheet_member_reference() {
    // Stage: the two-sheet workbook, a target, a fit (writes the lock and both sheet sidecars).
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_two_fit.xlsx"),
        dir.path().join("2025.xlsx"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("monat.tdy.sql"),
        "CREATE TABLE monat (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) WITH (files = '*.xlsx', date_order = 'dmy');",
    )
    .unwrap();
    let mut s = tdy::console::Session::new(dir.path().to_path_buf(), Default::default()).unwrap();
    let fit = s.run(".fit monat.tdy.sql").await.unwrap();
    assert!(fit.ok, "{}", fit.text);
    // Nothing to accept — but the member must be *found*, and the message must say so.
    let o = s.run(".accept monat.tdy.sql 2025.xlsx#Q1").await.unwrap();
    assert!(o.text.contains("nothing to accept") || o.text.contains("no judgement"), "{}", o.text);
    assert!(!o.text.contains("no fresh sidecar"), "the sheet sidecar must be the one loaded: {}", o.text);
}
```

If `Session::new`'s signature differs, copy the construction used by the existing tests in `tests/console.rs` (they all build a session the same way).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p tdy-tui --test workbench entering_a_sheet_member; cargo test --test console accept_resolves_a_sheet`
Expected: the workbench test fails with `sheet: None`; the console test fails with "no fresh sidecar" (it looked for `2025.xlsx#Q1.tdy.toml` under the name `2025.xlsx#Q1` as a *file*, or for the plain sidecar).

- [ ] **Step 3: Implement**

`src/console/mod.rs`, `Command::Accept`:

```rust
            Command::Accept { target, member } => {
                let pending = pending_accept;
                let t = self.resolve(&target)?;
                // A typed member is resolved against the lock: `book.xlsx#Q1` is
                // the sheet member if the lock has one, else a file of that name.
                let dir = crate::lockfile::target_dir(&t);
                let lock = crate::lockfile::Lock::load(&t)?;
                let known = |m: &crate::member::MemberRef| {
                    lock.as_ref().map(|l| l.member(&m.path, m.sheet.as_deref()).is_some()).unwrap_or(false)
                };
                let mref = crate::member::MemberRef::resolve(&member, known)
                    .unwrap_or_else(|| crate::member::MemberRef::file(member.clone()));
                let member_path = crate::fileio::confine(&dir.join(&mref.path), &self.root)
                    .with_context(|| member.clone())?;
                let same = pending.as_ref().map(|(pt, pm)| pt == &t && pm == &member).unwrap_or(false);
                if same {
                    let mut o = self.fit_pile(&t, &[PathBuf::from(&member)], false, false, progress).await?;
                    o.text = format!("accepted {member}\n\n{}", o.text);
                    return Ok(o);
                }
                let sc = match crate::sidecar::load_member(&member_path, mref.sheet.as_deref())? {
                    crate::sidecar::SidecarStatus::Fresh(sc) => sc,
                    _ => bail!("{member} has no fresh sidecar; run `.fit {target}` first"),
                };
                // ... unchanged from here ...
```

`tdy-tui/src/workbench.rs`, `enter_pile_member`: read the member's sheet alongside its path and pass it:

```rust
        let Some((member_path, member_sheet)) =
            report.members.get(selected).map(|m| (m.path.clone(), m.sheet.clone()))
        else { return WbAction::None };
        // ... existing context switch ...
        let preview_path = member_preview_path(&target, &member_path);
        self.preview_action(preview_path, member_sheet)
```

(`member_preview_path` is unchanged: `path` is the file.)

- [ ] **Step 4: Run the tests and the suite**

Run: `cargo test -p tdy-tui --test workbench; cargo test --test console; cargo test --workspace --lib --tests`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add src/console/mod.rs tdy-tui/src/workbench.rs tdy-tui/tests/workbench.rs tests/console.rs
git commit -m "The console's .accept and the workbench's preview know which sheet a member is"
```

---

### Task 8: Docs

**Files:**
- Modify: `CLAUDE.md` (the "Two questions are deferred" paragraph and a new paragraph after Slice 5)
- Modify: `README.md` ("Declaring the dataset you want", after the `tdy fit` pile example)
- Modify: `docs/design/2026-09-06-members-and-regions.md` (preamble)

- [ ] **Step 1: CLAUDE.md**

Replace the members-and-regions clause in the "Two questions are deferred" paragraph so it reads that only compressed inputs remain deferred, and add:

```markdown
**Workbook members are in (2026-09-07).** `docs/design/2026-09-07-workbook-members.md`.
A member is `(path, sheet: Option<String>)` — `src/member.rs`'s `MemberRef`, two fields in
the lock and the sidecar fingerprint, textual `book.xlsx#Q1` only where a person reads or
types it (report, `_member`, `--accept`, `.accept`, `exclude`), and a typed reference is
**resolved against the members that exist** (`MemberRef::resolve`), never split by rule.
A sheet member's sidecar is `<file>#<sheet>.tdy.toml` (`sidecar::load_member`/`save_member`;
the old forms are the `None` case). `fit_pile` asks `fit::discover_sheets` which sheets pass
the gates: none or one keeps a plain member (the `one_fits` fixture and every existing lock
are unchanged); several become one member per sheet, each fully fitted by `fit::fit_sheet`
and carrying a note naming the sheets that did not fit. The single-file `fit()` still refuses
several fitting sheets with `AmbiguousFrame` — "the spec for this file" has no single answer;
the pile is where "which members?" is asked. **Drift is per file**: the file's hash covers
every sheet, so a changed workbook is one `Changed` and the refit rediscovers the sheet set;
`Duplicated` is per (path, sheet). `dataset()` never lists a workbook's sheets itself.
Regions (several tables in one text file) are still deferred, behind the review gate.
```

- [ ] **Step 2: README.md**

After the `tdy fit sales.tdy.sql` pile example in "Declaring the dataset you want", add:

```markdown
A workbook with one sheet per period is the same pile in one file. `tdy fit`
frames every sheet and tries the declaration against each; when several
produce the declared table, each becomes a member of its own, named
`book.xlsx#Q1`, with its own sidecar beside the workbook (`book.xlsx#Q1.tdy.toml`)
and its own line in the lock. A sheet that does not fit is named in the notes,
`exclude = 'book.xlsx#Cover'` removes one by name, and a sheet added next year
changes the workbook's hash, so it is drift — named — and the next fit picks it
up. `source_name { from = "sheet" }` turns the sheet into a column.
```

- [ ] **Step 3: The design page preamble**

In `docs/design/2026-09-06-members-and-regions.md`, change the italic preamble's last sentence from "No code — this is the page the shape slice deferred to." to "Steps 1 and 2 of §4 landed on 2026-09-07 — `docs/design/2026-09-07-workbook-members.md`; regions (step 3) remain deferred."

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md docs/design/2026-09-06-members-and-regions.md
git commit -m "Docs: workbook members"
```

---

## Self-review

**Spec coverage.** §2 identity → Tasks 1, 3; §3 sidecars and lock, drift per file → Tasks 2, 3; §4 discovery table, notes, `fit()` unchanged, `exclude` by reference → Tasks 5, 6; §5 `dataset()`, report `sheet`, workbench preview, MCP via serde → Tasks 4, 6, 7; §6 the rule → Task 4's test reads only the lock; §7 tests → each named test appears in Tasks 3–7; the MCP `fit` result naming sheet members is covered by serde on `MemberReport` (Task 6) and the existing MCP test's member listing, which reads `resolved.members[].rel` (Task 4's `name()`).

**Placeholders.** None; every step carries its code. Task 6's substitution list is deliberate: the loop body is 200 lines of existing code and the change is mechanical.

**Type consistency.** `MemberRef::{file, sheet, name, resolve}` (Task 1) are the names used in Tasks 3, 6, 7. `sidecar_path_for/load_member/save_member` (Task 2) are used in Tasks 3, 4, 6, 7. `Lock::member(path, sheet)` and `spec_digest_for` (Task 3) are used in Tasks 4, 6, 7. `discover_sheets` returning `Result<Option<SheetDiscovery>, FitError>` and `fit_sheet` returning `Result<Fitted, FitError>` (Task 5) are used in Task 6, where `Planned { fitted, method, model }` matches `src/fit.rs:546`. `MemberReport.sheet` and `name()` (Task 6) are used in Task 7.
