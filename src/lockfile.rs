//! Which files are in the dataset, and what was proved about each.
//!
//! A target declares globs. A lock records what those globs *resolved to* the
//! last time somebody ran `tdy fit`, with each member's hash and verdict.
//! Queries read the lock; they never expand a glob.
//!
//! That is the difference between a reproducible dataset and a directory
//! listing. If `dataset()` expanded the glob at query time, the answer would
//! depend on what happens to be in the folder — the same query over the same
//! declaration would return different numbers the morning after an export
//! landed, with nothing to point at. `--frozen` would be unimplementable, and
//! the file that changed the total would be invisible.
//!
//! So a new file arriving is **drift**: `tdy check` reports it and exits
//! non-zero, `dataset()` refuses, and `tdy fit` is what settles it. December's
//! export breaks the build, loudly, instead of silently changing a number.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::target::Target;

/// One file that belongs to the dataset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Member {
    /// Relative to the directory holding the target file, so a checked-out
    /// repo works wherever it is cloned.
    pub path: String,
    /// One sheet of a workbook, when a workbook contributes several
    /// members; absent for a plain member. Two fields rather than a
    /// `path#sheet` string, so a `#` in either name cannot make the lock
    /// ambiguous (`crate::member::MemberRef`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// One of several tables stacked in the file or sheet, counted from 1;
    /// absent for a member covering the whole file or sheet. See
    /// `crate::member::MemberRef::region`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<u32>,
    pub blake3: String,
    pub bytes: u64,
    /// Fingerprint of the *spec*, not the data.
    ///
    /// An acceptance is a judgement about a plan, and the plan lives in the
    /// sidecar. Recording only the file's hash meant a sidecar could be
    /// hand-edited after acceptance — adding a `decimal_shift`, say — and the
    /// dataset would keep running it as accepted. The data had not changed, so
    /// nothing else noticed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub spec_digest: String,
    /// Why this member needed a human's judgement, if it did.
    ///
    /// A plan whose acceptance rests on a *semantic* judgement rather than a
    /// mechanical proof does not run until somebody accepts it. The classic
    /// case is a unit shift: `decimal_shift = -2` is exact, lossless and
    /// self-evidencing, and it is still a claim that this file's numbers mean
    /// something other than what they say. No proof can settle that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    /// Set by `tdy fit --accept`. Expires on its own: the acceptance lives in
    /// this entry, and drift replaces the entry whenever the file's bytes or
    /// the declaration change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accepted: bool,
}

impl Member {
    /// The form a person reads and types.
    pub fn name(&self) -> String {
        crate::member::MemberRef { path: self.path.clone(), sheet: self.sheet.clone(), region: self.region }.name()
    }
}

/// The resolved membership of a dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub lock_version: u32,
    /// The dataset's name, for a readable error when the wrong lock is found.
    pub target: String,
    /// Fingerprint of the target's *meaning* — see [`target_hash`].
    pub target_hash: String,
    pub tool_version: String,
    pub created_at: String,
    #[serde(default, rename = "member")]
    pub members: Vec<Member>,
}

pub const LOCK_VERSION: u32 = 1;

/// `sales.tdy.sql` -> `sales.tdy.lock`
pub fn lock_path(target_file: &Path) -> PathBuf {
    let mut p = target_file.to_path_buf();
    let stem = target_file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let base = stem.strip_suffix(".sql").unwrap_or(&stem).to_string();
    p.set_file_name(format!("{base}.lock"));
    p
}

/// A fingerprint of what the target *means*, not of its bytes.
///
/// Comments and whitespace are where a target file gets most of its edits —
/// the point of writing it in SQL is that it reads like documentation — and
/// invalidating every member's proof because somebody clarified a comment
/// would train people to ignore the invalidation. So this hashes the
/// declaration: the name, each column's name, type, nullability and match
/// list, and the options that change how planning happens.
pub fn target_hash(t: &Target) -> String {
    let mut h = blake3::Hasher::new();
    h.update(t.name.as_bytes());
    for c in &t.columns {
        h.update(b"\x1fcol");
        h.update(c.name.as_bytes());
        h.update(format!("{:?}", c.dtype).as_bytes());
        h.update(if c.nullable { b"?" } else { b"!" });
        // A fill declared or retracted changes what queries return for a
        // member that lacks the column, so it must void the proofs.
        h.update(if c.if_missing_null { b"~" } else { b"." });
        h.update(if c.round { b"r" } else { b"." });
        for m in &c.matches {
            h.update(b"\x1f");
            h.update(m.as_bytes());
        }
    }
    for f in &t.files {
        h.update(b"\x1ff");
        h.update(f.as_bytes());
    }
    for f in &t.exclude {
        h.update(b"\x1fx");
        h.update(f.as_bytes());
    }
    h.update(
        format!(
            "{:?}{:?}{:?}{:?}{:?}{:?}",
            t.match_mode,
            t.date_order,
            t.verify,
            t.timezone,
            t.decimal_separator,
            // Turning provenance on adds two columns to what every query sees,
            // so the proofs taken against the narrower shape are no longer
            // about this dataset.
            t.provenance
        )
        .as_bytes(),
    );
    format!("b3:{}", h.finalize().to_hex())
}

impl Lock {
    pub fn load(target_file: &Path) -> Result<Option<Lock>> {
        let p = lock_path(target_file);
        if !p.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&p)
            .with_context(|| format!("cannot read {}", p.display()))?;
        let lock: Lock = toml::from_str(&text)
            .with_context(|| format!("{} is not a valid lock file", p.display()))?;
        if lock.lock_version != LOCK_VERSION {
            anyhow::bail!(
                "{} was written by a different version of tdy (lock_version {}, this build \
                 understands {LOCK_VERSION}). Re-run `tdy fit`.",
                p.display(),
                lock.lock_version
            );
        }
        Ok(Some(lock))
    }

    pub fn save(&self, target_file: &Path) -> Result<PathBuf> {
        let p = lock_path(target_file);
        let body = format!(
            "# Written by `tdy fit`. Reviewed in git, never hand-edited.\n\
             # Membership is recorded here rather than expanded from the target's globs at\n\
             # query time, so the same query over the same files gives the same answer and a\n\
             # new export is drift rather than a silently different number.\n\
             # On a merge conflict, do not hand-merge: this file is derived state, and\n\
             # `tdy fit <TARGET>` regenerates it from the declaration and the files on disk.\n\n{}",
            toml::to_string_pretty(self).context("serialising the lock")?
        );
        crate::fileio::atomic_write(&p, &body)?;
        Ok(p)
    }

    pub fn member(&self, path: &str, sheet: Option<&str>, region: Option<u32>) -> Option<&Member> {
        self.members
            .iter()
            .find(|m| m.path == path && m.sheet.as_deref() == sheet && m.region == region)
    }
}

/// How the world differs from what the lock recorded.
#[derive(Debug, Clone, PartialEq)]
pub enum Drift {
    /// A file matches the target's globs and is not in the lock.
    Added(String),
    /// A file is in the lock and no longer on disk.
    Removed(String),
    /// A file's contents changed since it was fitted — one drift for the
    /// file, covering every member of it.
    Changed(String),
    /// The declaration itself changed, so every member's proof is void.
    TargetChanged,
    /// The same member is listed twice.
    Duplicated(String),
    /// One file is listed both whole and by sheet, so its rows would be read
    /// twice — once entire, once per sheet.
    MixedGranularity(String),
    /// The data is unchanged but its spec was edited after it was fitted.
    SpecEdited(String),
}

/// Fingerprint of a member's spec, as stored in its sidecar.
///
/// Empty when the sidecar cannot be read: a missing spec is reported by the
/// dataset's own load, and inventing a digest here would turn that into a
/// confusing drift message instead.
pub fn spec_digest(data_file: &Path) -> String {
    spec_digest_for(data_file, None, None)
}

/// Fingerprint of a member's spec, as stored in its sidecar — a workbook
/// member's sidecar is per sheet, and a stacked member's is per region
/// (`sidecar::sidecar_path_for`), so both must be named to find the right
/// one.
pub fn spec_digest_for(data_file: &Path, sheet: Option<&str>, region: Option<u32>) -> String {
    let p = crate::sidecar::sidecar_path_for(data_file, sheet, region);
    match std::fs::read(&p) {
        Ok(bytes) => format!("b3:{}", blake3::hash(&bytes).to_hex()),
        Err(_) => String::new(),
    }
}

impl Drift {
    pub fn message(&self) -> String {
        match self {
            Drift::Added(p) => format!(
                "{p} matches this dataset and is not in the lock — run `tdy fit` to plan it"
            ),
            Drift::Removed(p) => format!(
                "{p} is in the lock and no longer on disk — run `tdy fit` to drop it"
            ),
            Drift::Changed(p) => format!(
                "{p} has changed since it was fitted — run `tdy fit` to re-plan it"
            ),
            Drift::Duplicated(p) => {
                format!("{p} is listed twice in the lock — run `tdy fit` to rebuild it")
            }
            Drift::MixedGranularity(p) => format!(
                "{p} is listed both whole and in parts (by sheet or by region), so its rows \
                 would be read twice — run `tdy fit` to rebuild it"
            ),
            Drift::SpecEdited(p) => format!(
                "{p}'s spec was edited after it was accepted — the acceptance was given to \
                 the plan as it read then. Re-accept it:  tdy fit <TARGET> --accept {p}"
            ),
            Drift::TargetChanged => {
                "the target declaration changed, so every member must be re-fitted — \
                 run `tdy fit`"
                    .to_string()
            }
        }
    }
}

/// Compare the lock against the files the target's globs resolve to now.
pub fn drift(lock: &Lock, target: &Target, target_file: &Path) -> Result<Vec<Drift>> {
    let mut out = Vec::new();
    if lock.target_hash != target_hash(target) {
        // Everything downstream is void, and listing per-file drift on top
        // would be noise about proofs that no longer mean anything.
        return Ok(vec![Drift::TargetChanged]);
    }

    let dir = target_dir(target_file);
    let on_disk = resolve(target, target_file)?;
    let locked_files: BTreeSet<&str> = lock.members.iter().map(|m| m.path.as_str()).collect();

    // Two sheets of one workbook, or two regions of one file, are distinct
    // members; the same (path, sheet, region) twice would be read twice.
    {
        let mut seen: BTreeSet<(&str, Option<&str>, Option<u32>)> = BTreeSet::new();
        for m in &lock.members {
            if !seen.insert((m.path.as_str(), m.sheet.as_deref(), m.region)) {
                out.push(Drift::Duplicated(m.name()));
            }
        }
    }

    for rel in &on_disk {
        if locked_files.contains(rel.as_str()) {
            continue;
        }
        // A file every one of whose members the declaration excludes by name
        // is accounted for: it is absent from the lock *because* the target
        // says so, and reporting it as `Added` on every query would make a
        // deliberate exclusion look like unplanned drift for ever.
        let excluded_by_ref = target
            .exclude
            .iter()
            .any(|x| x.strip_prefix(rel.as_str()).is_some_and(|s| s.starts_with('#')));
        if !excluded_by_ref {
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
        // A whole-file member covers every sheet the file has, so listing it
        // beside a sheet member of the same file reads those rows twice and
        // sums to a plausible wrong number. `fit` cannot produce this — a
        // file is expanded or it is not — but a lock is a text file. The
        // same is true one level down: within one (path, sheet) — including
        // the file's own "no sheet" group — a member covering the whole of
        // it beside one or more members covering only a region of it reads
        // rows twice again, grouped by (path, sheet) because a sheet's
        // regions and another sheet's regions must never be confused.
        let has_whole_file = members.iter().any(|m| m.sheet.is_none() && m.region.is_none());
        let has_sheet_members = members.iter().any(|m| m.sheet.is_some());
        let has_region_mix = {
            let mut by_sheet: Vec<(Option<&str>, Vec<&Member>)> = Vec::new();
            for m in &members {
                match by_sheet.iter_mut().find(|(s, _)| *s == m.sheet.as_deref()) {
                    Some((_, ms)) => ms.push(m),
                    None => by_sheet.push((m.sheet.as_deref(), vec![m])),
                }
            }
            by_sheet.iter().any(|(_, ms)| {
                ms.iter().any(|m| m.region.is_none()) && ms.iter().any(|m| m.region.is_some())
            })
        };
        if (has_whole_file && has_sheet_members) || has_region_mix {
            out.push(Drift::MixedGranularity(path.to_string()));
        }
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
        // The spec is the thing that was reviewed, so it is the thing whose
        // change must invalidate the review.
        //
        // Only for an *accepted* member. A sidecar is a human assertion about
        // specific bytes and tdy honours it — `fit` keeps a hand-written spec
        // rather than replanning it — so an edit is not by itself drift, and
        // conformance plus the dry run still gate it on every load. What an
        // edit must not survive is an acceptance, because the acceptance was
        // given to the spec as it read then.
        for m in members {
            if m.accepted
                && !m.spec_digest.is_empty()
                && spec_digest_for(&p, m.sheet.as_deref(), m.region) != m.spec_digest
            {
                out.push(Drift::SpecEdited(m.name()));
            }
        }
    }
    Ok(out)
}

/// The directory a target's globs are relative to.
pub fn target_dir(target_file: &Path) -> PathBuf {
    // `Path::parent` of a bare filename is `Some("")`, not `None`, and
    // `read_dir("")` fails — so `tdy fit sales.tdy.sql` from inside the data
    // directory resolved to no members at all, which made every drift check
    // vacuously pass.
    match target_file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Files the target's globs match now, relative to the target's directory,
/// in sorted order.
///
/// Sorted because the union reads members in this order and the row order of
/// a dataset must not depend on how a directory happens to be laid out.
pub fn resolve(target: &Target, target_file: &Path) -> Result<Vec<String>> {
    resolve_excluded(target, target_file).map(|(rels, _)| rels)
}

/// The same, plus which `exclude` entry removed which file.
///
/// `exclude` is applied twice — here as a glob over file names, and again in
/// `report::expand_units` as an exact member reference, which is the only
/// place a member reference *can* be applied. One entry can match in both
/// passes (`report.csv#2` is a file's name and block 2 of `report.csv`), and
/// then it drops two different things at once. The caller cannot see that
/// without knowing what this pass removed, and a second directory walk to
/// find out would be a walk that could disagree with this one.
pub fn resolve_excluded(
    target: &Target,
    target_file: &Path,
) -> Result<(Vec<String>, Vec<(String, String)>)> {
    let dir = target_dir(target_file);
    let mut out: BTreeSet<String> = BTreeSet::new();

    for pat in &target.files {
        let (sub, name_pat) = split_pattern(pat);
        let search = if sub.is_empty() { dir.clone() } else { dir.join(&sub) };
        let Ok(rd) = std::fs::read_dir(&search) else { continue };
        for e in rd.flatten() {
            if !e.path().is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if !matches_glob(&name_pat, &name) {
                continue;
            }
            // A sidecar or a lock beside the data is not data.
            if name.ends_with(".tdy.toml") || name.ends_with(".tdy.lock") || name.ends_with(".tdy.sql") {
                continue;
            }
            let rel = if sub.is_empty() { name.clone() } else { format!("{sub}/{name}") };
            out.insert(rel);
        }
    }

    let mut removed: Vec<(String, String)> = Vec::new();
    for pat in &target.exclude {
        let (sub, name_pat) = split_pattern(pat);
        out.retain(|rel| {
            let (rsub, rname) = split_pattern(rel);
            // An exclude naming no directory applies to every directory —
            // `exclude = '*-draft.csv'` should mean what it says, and
            // requiring it to repeat the files= prefix made it silently do
            // nothing.
            let dir_matches = sub.is_empty() || rsub == sub;
            let hit = dir_matches && matches_glob(&name_pat, &rname);
            if hit {
                removed.push((pat.clone(), rel.clone()));
            }
            !hit
        });
    }

    Ok((out.into_iter().collect(), removed))
}

/// One pattern against one directory, for a caller with no shell in front of
/// it (the console). `*` and `?` in the final path segment only, the same
/// matcher `files =` uses, so a glob means the same thing in both places.
///
/// A literal (no `*`/`?`) is returned as-is whether or not it exists: the
/// command that opens it reports "file not found", which is more useful than
/// "no match". Sidecars and locks are never data, so they are skipped even
/// when `*` would match them.
pub fn expand_glob(dir: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let (sub, name_pat) = split_pattern(pattern);
    let search = if sub.is_empty() { dir.to_path_buf() } else { dir.join(&sub) };
    if !name_pat.contains(['*', '?']) {
        return Ok(vec![search.join(&name_pat)]);
    }
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    let rd = std::fs::read_dir(&search)
        .with_context(|| format!("cannot read directory {}", search.display()))?;
    for e in rd.flatten() {
        if !e.path().is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if !matches_glob(&name_pat, &name) {
            continue;
        }
        if name.ends_with(".tdy.toml") || name.ends_with(".tdy.lock") {
            continue;
        }
        out.insert(search.join(name));
    }
    Ok(out.into_iter().collect())
}

/// `exports/2025-*.csv` -> ("exports", "2025-*.csv")
fn split_pattern(p: &str) -> (String, String) {
    match p.rsplit_once('/') {
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), p.to_string()),
    }
}

/// `*` and `?` against a file name. Deliberately not a full glob library: a
/// dataset's members live beside its target, and `**` recursion would make
/// membership depend on a directory tree nobody is looking at.
fn matches_glob(pat: &str, name: &str) -> bool {
    // Iterative, with a single remembered star. The recursive version branched
    // at every `*` and took exponential time on a pattern like `*a*a*a*a*b`
    // against a long run of `a`s — a filename nobody would write on purpose,
    // but `files=` is user input and a glob is not a place to hang. This is the
    // standard one-star backtrack: O(pattern x name) worst case, linear in
    // practice, and it cannot overflow the stack.
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    // Where to resume if the current star turns out to have eaten too little.
    let mut star: Option<(usize, usize)> = None;

    loop {
        if ni < n.len() && pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            // Give the star one more character and try again from there.
            if sn >= n.len() {
                return false;
            }
            star = Some((sp, sn + 1));
            pi = sp + 1;
            ni = sn + 1;
        } else {
            return false;
        }
        if ni == n.len() {
            // Trailing stars match nothing, which is still a match.
            return p[pi..].iter().all(|c| *c == '*');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sql: &str) -> Target {
        Target::parse(sql).unwrap()
    }

    fn m(path: &str, sheet: Option<&str>) -> Member {
        Member {
            path: path.into(),
            sheet: sheet.map(str::to_string),
            region: None,
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
        assert!(back.member("a.xlsx", Some("Q1"), None).is_some());
        assert!(back.member("a.xlsx", None, None).is_none(), "the plain member of that file does not exist");
    }

    /// A file listed both whole and by sheet would be read twice — once
    /// entire, once per sheet — and the sum would be plausible.
    #[test]
    fn a_file_listed_both_whole_and_by_sheet_is_drift() {
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
        let fresh = |sheet: Option<&str>| Member {
            blake3: hash.clone(),
            bytes,
            ..m("book.xlsx", sheet)
        };
        let lock = Lock {
            lock_version: LOCK_VERSION,
            target: "t".into(),
            target_hash: target_hash(&target),
            tool_version: "0".into(),
            created_at: "now".into(),
            members: vec![fresh(None), fresh(Some("Q1"))],
        };
        let d1 = drift(&lock, &target, &t).unwrap();
        assert!(
            matches!(&d1[..], [Drift::MixedGranularity(n)] if n == "book.xlsx"),
            "{d1:?}"
        );
        assert!(d1[0].message().contains("whole") && d1[0].message().contains("sheet"), "{}", d1[0].message());
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

        // Two regions of one file are two members too — no drift.
        let fresh_region = |region: u32| Member { blake3: hash.clone(), bytes, region: Some(region), ..m("book.xlsx", None) };
        let region_lock = Lock {
            lock_version: LOCK_VERSION,
            target: "t".into(),
            target_hash: target_hash(&target),
            tool_version: "0".into(),
            created_at: "now".into(),
            members: vec![fresh_region(1), fresh_region(2)],
        };
        assert!(drift(&region_lock, &target, &t).unwrap().is_empty(), "two regions of one file are two members");

        // A whole-file member plus a region member of the same file reads
        // its rows twice, exactly as a whole-file-plus-sheet mix would.
        let mixed_lock = Lock {
            members: vec![Member { blake3: hash.clone(), bytes, ..m("book.xlsx", None) }, fresh_region(1)],
            ..region_lock
        };
        let dm = drift(&mixed_lock, &target, &t).unwrap();
        assert!(matches!(&dm[..], [Drift::MixedGranularity(n)] if n == "book.xlsx"), "{dm:?}");
        // The message must not claim "by sheet" for a whole-file/region mix,
        // which never involved a sheet at all.
        assert!(
            dm[0].message().contains("whole") && dm[0].message().contains("region"),
            "{}",
            dm[0].message()
        );

        // Touch the workbook: every sheet member's proof is void, said once.
        let mut bytes_on_disk = std::fs::read(&book).unwrap();
        bytes_on_disk.push(0);
        std::fs::write(&book, bytes_on_disk).unwrap();
        let d2 = drift(&lock, &target, &t).unwrap();
        assert_eq!(d2, vec![Drift::Changed("book.xlsx".into())], "{d2:?}");
    }

    #[test]
    fn a_region_member_round_trips_and_mixed_granularity_covers_regions() {
        let mut r2 = m("report.csv", None);
        r2.region = Some(2);
        let lock = Lock { lock_version: LOCK_VERSION, target: "t".into(), target_hash: "b3:t".into(), tool_version: "0".into(), created_at: "now".into(), members: vec![r2.clone(), m("b.csv", None)] };
        let text = toml::to_string_pretty(&lock).unwrap();
        assert_eq!(text.matches("region = 2").count(), 1, "{text}");
        let back: Lock = toml::from_str(&text).unwrap();
        assert_eq!(back.members[0].name(), "report.csv#2");
        assert!(back.member("report.csv", None, Some(2)).is_some());
        assert!(back.member("report.csv", None, None).is_none());
    }

    #[test]
    fn glob_matching_is_ordinary() {
        assert!(matches_glob("2025-*.csv", "2025-01.csv"));
        assert!(matches_glob("*.csv", "a.csv"));
        assert!(matches_glob("*", "anything"));
        assert!(matches_glob("2025-??.csv", "2025-01.csv"));
        assert!(!matches_glob("2025-*.csv", "2024-01.csv"));
        assert!(!matches_glob("2025-??.csv", "2025-1.csv"));
        assert!(!matches_glob("*.csv", "a.csv.bak"));
        // A pattern with several stars must not blow up.
        assert!(matches_glob("*-*-*.csv", "a-b-c.csv"));
    }

    /// The recursive matcher branched at every star and took exponential time
    /// on this. `files=` is user input, and a glob is not a place to hang.
    #[test]
    fn a_pathological_glob_does_not_hang() {
        let name = "a".repeat(64);
        let t = std::time::Instant::now();
        assert!(!matches_glob("*a*a*a*a*a*a*a*b", &name));
        assert!(matches_glob("*a*a*a*a*a*a*a*a", &name));
        assert!(t.elapsed() < std::time::Duration::from_millis(100), "{:?}", t.elapsed());
    }

    /// `Path::parent` of a bare filename is `Some("")`, not `None`, and
    /// `read_dir("")` fails — so a target named without a directory resolved
    /// to no members and every drift check passed vacuously.
    #[test]
    fn a_bare_target_filename_has_a_usable_directory() {
        assert_eq!(target_dir(Path::new("sales.tdy.sql")), PathBuf::from("."));
        assert_eq!(target_dir(Path::new("d/sales.tdy.sql")), PathBuf::from("d"));
    }

    #[test]
    fn the_lock_path_sits_beside_the_target() {
        assert_eq!(
            lock_path(Path::new("exports/sales.tdy.sql")),
            PathBuf::from("exports/sales.tdy.lock")
        );
    }

    /// A comment is where a SQL target gets most of its edits — the point of
    /// writing it in SQL is that it reads like documentation. Invalidating
    /// twelve proofs because someone clarified one would train people to
    /// ignore the invalidation.
    #[test]
    fn the_target_hash_ignores_comments_and_layout() {
        let a = t("CREATE TABLE s (a TEXT NOT NULL) WITH (files='x.csv')");
        let b = t("-- a comment nobody should be punished for\n\
                   CREATE TABLE s (\n  a   TEXT   NOT NULL\n)\nWITH (files = 'x.csv');");
        assert_eq!(target_hash(&a), target_hash(&b));
    }

    /// …but anything that changes what the dataset *is* must invalidate it.
    #[test]
    fn the_target_hash_moves_when_the_declaration_does() {
        let base = t("CREATE TABLE s (a TEXT NOT NULL) WITH (files='x.csv')");
        for changed in [
            "CREATE TABLE s (a TEXT NULL) WITH (files='x.csv')",
            "CREATE TABLE s (a BIGINT NOT NULL) WITH (files='x.csv')",
            "CREATE TABLE s (b TEXT NOT NULL) WITH (files='x.csv')",
            "CREATE TABLE other (a TEXT NOT NULL) WITH (files='x.csv')",
            "CREATE TABLE s (a TEXT NOT NULL) WITH (files='y.csv')",
            "CREATE TABLE s (a TEXT NOT NULL) WITH (files='x.csv', date_order='mdy')",
            "CREATE TABLE s (a TEXT NOT NULL OPTIONS(matches='A')) WITH (files='x.csv')",
        ] {
            assert_ne!(
                target_hash(&base),
                target_hash(&t(changed)),
                "this change did not move the hash: {changed}"
            );
        }
    }

    #[test]
    fn resolving_globs_is_sorted_and_skips_tdy_files() {
        let dir = tempfile::TempDir::new().unwrap();
        for n in ["2025-02.csv", "2025-01.csv", "2024-12.csv", "notes.txt"] {
            std::fs::write(dir.path().join(n), "x").unwrap();
        }
        // The target, its lock and a sidecar must never become members.
        std::fs::write(dir.path().join("s.tdy.sql"), "x").unwrap();
        std::fs::write(dir.path().join("s.tdy.lock"), "x").unwrap();
        std::fs::write(dir.path().join("2025-01.csv.tdy.toml"), "x").unwrap();

        let target = t("CREATE TABLE s (a TEXT) WITH (files = '2025-*.csv, *.txt')");
        let got = resolve(&target, &dir.path().join("s.tdy.sql")).unwrap();
        assert_eq!(got, vec!["2025-01.csv", "2025-02.csv", "notes.txt"]);
    }

    #[test]
    fn exclude_removes_members() {
        let dir = tempfile::TempDir::new().unwrap();
        for n in ["2025-01.csv", "2025-02-entwurf.csv"] {
            std::fs::write(dir.path().join(n), "x").unwrap();
        }
        let target =
            t("CREATE TABLE s (a TEXT) WITH (files = '2025-*.csv', exclude = '*-entwurf.csv')");
        let got = resolve(&target, &dir.path().join("s.tdy.sql")).unwrap();
        assert_eq!(got, vec!["2025-01.csv"]);
    }

    #[test]
    fn expand_glob_matches_files_sorted_and_skips_companions() {
        let d = tempfile::tempdir().unwrap();
        // Pattern `a*` matches a.csv, a.csv.tdy.toml, and a.tdy.lock, but the latter
        // two should be filtered by the companion-skip logic since they end with
        // .tdy.toml or .tdy.lock.
        for n in ["b.csv", "a.csv", "a.csv.tdy.toml", "a.tdy.lock", "x.txt"] {
            std::fs::write(d.path().join(n), "").unwrap();
        }
        std::fs::create_dir(d.path().join("sub.csv")).unwrap(); // a dir, not a file
        let got = expand_glob(d.path(), "a*").unwrap();
        let names: Vec<String> =
            got.iter().map(|p| p.file_name().unwrap().to_string_lossy().into()).collect();
        assert_eq!(names, ["a.csv"]);
        assert!(got.iter().all(|p| p.starts_with(d.path())));
    }

    #[test]
    fn expand_glob_literal_passes_through_even_if_absent() {
        let d = tempfile::tempdir().unwrap();
        let got = expand_glob(d.path(), "missing.csv").unwrap();
        assert_eq!(got, vec![d.path().join("missing.csv")]);
        assert!(expand_glob(d.path(), "nothing-*.csv").unwrap().is_empty());
    }

    #[test]
    fn expand_glob_with_subdirectory() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("exports")).unwrap();
        std::fs::write(d.path().join("exports/2025-01.csv"), "").unwrap();
        let got = expand_glob(d.path(), "exports/2025-*.csv").unwrap();
        assert_eq!(got, vec![d.path().join("exports/2025-01.csv")]);
    }

    /// Declaring or retracting rounding changes what a member may do, so it
    /// voids the proofs, as `if_missing` does.
    #[test]
    fn round_is_part_of_the_targets_meaning() {
        let a = crate::target::Target::parse("CREATE TABLE t (a DECIMAL(14,2)) WITH (files = '*.csv')").unwrap();
        let b = crate::target::Target::parse(
            "CREATE TABLE t (a DECIMAL(14,2) OPTIONS(round = 'half_away')) WITH (files = '*.csv')",
        )
        .unwrap();
        assert_ne!(target_hash(&a), target_hash(&b));
    }
}
