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
    pub blake3: String,
    pub bytes: u64,
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
    h.update(format!("{:?}{:?}{:?}{:?}", t.match_mode, t.date_order, t.verify, t.timezone).as_bytes());
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
             # new export is drift rather than a silently different number.\n\n{}",
            toml::to_string_pretty(self).context("serialising the lock")?
        );
        crate::fileio::atomic_write(&p, &body)?;
        Ok(p)
    }

    pub fn member(&self, path: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.path == path)
    }
}

/// How the world differs from what the lock recorded.
#[derive(Debug, Clone, PartialEq)]
pub enum Drift {
    /// A file matches the target's globs and is not in the lock.
    Added(String),
    /// A file is in the lock and no longer on disk.
    Removed(String),
    /// A member's contents changed since it was fitted.
    Changed(String),
    /// The declaration itself changed, so every member's proof is void.
    TargetChanged,
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
    let locked: BTreeSet<&str> = lock.members.iter().map(|m| m.path.as_str()).collect();

    for rel in &on_disk {
        if !locked.contains(rel.as_str()) {
            out.push(Drift::Added(rel.clone()));
        }
    }
    for m in &lock.members {
        let p = dir.join(&m.path);
        if !p.exists() {
            out.push(Drift::Removed(m.path.clone()));
            continue;
        }
        let (hash, bytes) = crate::sidecar::hash_file(&p)?;
        if hash != m.blake3 || bytes != m.bytes {
            out.push(Drift::Changed(m.path.clone()));
        }
    }
    Ok(out)
}

/// The directory a target's globs are relative to.
pub fn target_dir(target_file: &Path) -> PathBuf {
    target_file.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
}

/// Files the target's globs match now, relative to the target's directory,
/// in sorted order.
///
/// Sorted because the union reads members in this order and the row order of
/// a dataset must not depend on how a directory happens to be laid out.
pub fn resolve(target: &Target, target_file: &Path) -> Result<Vec<String>> {
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

    for pat in &target.exclude {
        let (sub, name_pat) = split_pattern(pat);
        out.retain(|rel| {
            let (rsub, rname) = split_pattern(rel);
            !(rsub == sub && matches_glob(&name_pat, &rname))
        });
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
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => {
                // Match the rest here, or consume one char and retry.
                go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..]))
            }
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    go(&pat.chars().collect::<Vec<_>>(), &name.chars().collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sql: &str) -> Target {
        Target::parse(sql).unwrap()
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
}
