//! Where a member's plan is kept, and the one question every caller asks:
//! "what is this member's spec?"
//!
//! A target declares `plans = 'sidecars'` (the default: one `<file>.tdy.toml`
//! per member) or `plans = 'lock'` (one table of distinct plans in the lock,
//! each member naming the one it uses). A pile of 7,443 documents that share
//! one plan wrote 348 MB of identical sidecars and parsed them on every query;
//! the lock holds that plan once.
//!
//! The rule is the same everywhere, and [`Plans::plan_for`] is the only place
//! it is written down: **the member's sidecar file wins when it exists** —
//! that is how one member gets a different plan (`method = "manual"`) — and
//! otherwise the spec the member's lock entry names is its plan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::lockfile::{Lock, Member};
use crate::member::MemberRef;
use crate::spec::{InferenceMethod, ParseSpec, Sidecar};

/// The identity of a plan: blake3 over the spec's canonical serialisation —
/// the sidecar's own serialiser — with `notes` cleared.
///
/// Notes are free text about one fit of one file ("frame proved by
/// elimination: of 4 candidate frames…"), never machine-interpreted; they
/// are the only thing that differed across the 7,443 villagerdb plans.
/// Everything else — extraction, transforms, every column, `confidence` — is
/// what the plan *does*, and two members share a plan exactly when those
/// bytes are equal.
pub fn spec_id(spec: &ParseSpec) -> Result<String> {
    let bare = ParseSpec { notes: Vec::new(), ..spec.clone() };
    let text = toml::to_string_pretty(&bare).context("serialising a spec to identify it")?;
    Ok(format!("b3:{}", blake3::hash(text.as_bytes()).to_hex()))
}

/// Where a member's plan was read from.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    /// Its own sidecar file, at this path.
    Sidecar(PathBuf),
    /// The lock's spec table, entry `id`.
    Lock { lock: PathBuf, id: String },
}

/// Whether the plan is about the bytes on disk now.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// A sidecar whose fingerprint matches the file.
    Fresh,
    /// A sidecar written for other bytes.
    Stale,
    /// A lock-held plan, recorded against these bytes. Whether they are the
    /// bytes on disk is the drift check's question (`lockfile::drift`), so
    /// a caller that has just run it pays no second hash; one that has not
    /// asks [`Plan::is_fresh`].
    AsLocked { blake3: String, bytes: u64 },
}

/// One member's plan, wherever it is kept.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Shared, never copied per member: for a lock-held plan every member
    /// naming the entry holds the same `Arc`. Its `notes` are the entry's
    /// shared notes for a lock-held plan, the whole list for a sidecar.
    pub spec: Arc<ParseSpec>,
    /// A lock-held plan's notes particular to this member, which follow
    /// `spec.notes`; empty for a sidecar. [`Plan::notes`] is the whole list.
    pub own_notes: Vec<String>,
    pub method: InferenceMethod,
    pub model: Option<String>,
    pub origin: Origin,
    /// What an acceptance is tied to: blake3 of the sidecar file's bytes, or
    /// the spec identity of the lock entry's content, recomputed — so a
    /// hand-edited entry no longer matches its own id.
    pub digest: String,
    pub state: State,
}

impl Plan {
    /// The member's notes, in order.
    pub fn notes(&self) -> Vec<String> {
        let mut out = self.spec.notes.clone();
        out.extend(self.own_notes.iter().cloned());
        out
    }

    pub fn in_lock(&self) -> bool {
        matches!(self.origin, Origin::Lock { .. })
    }

    /// The id a lock-held plan is recorded under; `None` for a sidecar.
    pub fn lock_id(&self) -> Option<&str> {
        match &self.origin {
            Origin::Lock { id, .. } => Some(id),
            Origin::Sidecar(_) => None,
        }
    }

    /// A lock-held plan whose content no longer hashes to its id: the lock
    /// was edited by hand. Never true of a sidecar, whose edits are a
    /// workflow (and are what `--stamp` and `manual` are for).
    pub fn edited(&self) -> bool {
        matches!(&self.origin, Origin::Lock { id, .. } if *id != self.digest)
    }

    /// Is the plan about the file's current bytes?
    pub fn is_fresh(&self, file: &Path) -> Result<bool> {
        Ok(match &self.state {
            State::Fresh => true,
            State::Stale => false,
            State::AsLocked { blake3, bytes } => {
                let (h, n) = crate::sidecar::hash_file(file)?;
                h == *blake3 && n == *bytes
            }
        })
    }

    /// Where the plan lives, as a person reads it.
    pub fn whereabouts(&self) -> String {
        match &self.origin {
            Origin::Sidecar(p) => p.display().to_string(),
            Origin::Lock { lock, id } => format!("{} (spec {})", lock.display(), short_id(id)),
        }
    }
}

/// `b3:9c1f…` — enough of an id to tell two apart in a message.
pub fn short_id(id: &str) -> String {
    id.chars().take(3 + 12).collect::<String>() + "…"
}

/// A lock's spec table, parsed, validated and identified once per distinct
/// spec — not once per member — and an index of its members.
pub struct Plans<'a> {
    lock: Option<&'a Lock>,
    lock_path: PathBuf,
    held: HashMap<&'a str, std::result::Result<Held, String>>,
    index: HashMap<MemberKey<'a>, &'a Member>,
}

/// A lock member's (path, sheet, region), borrowed.
type MemberKey<'a> = (&'a str, Option<&'a str>, Option<u32>);

struct Held {
    spec: Arc<ParseSpec>,
    digest: String,
    method: InferenceMethod,
    model: Option<String>,
}

impl<'a> Plans<'a> {
    /// The plans `target_file`'s lock holds, if it has one.
    pub fn new(target_file: &Path, lock: Option<&'a Lock>) -> Plans<'a> {
        let mut held = HashMap::new();
        let mut index = HashMap::new();
        if let Some(l) = lock {
            for e in &l.specs {
                // A lock is text, so its spec table is untrusted input in
                // exactly the way a sidecar is: validated before anything
                // relies on it, once per entry.
                let h = match e.spec.validate() {
                    Err(errs) => Err(format!(
                        "the lock's plan {} is not a valid parsing spec:\n- {}",
                        short_id(&e.id),
                        errs.join("\n- ")
                    )),
                    Ok(()) => spec_id(&e.spec).map_err(|x| format!("{x:#}")).map(|digest| Held {
                        spec: Arc::new(e.spec.clone()),
                        digest,
                        method: e.method,
                        model: e.model.clone(),
                    }),
                };
                held.insert(e.id.as_str(), h);
            }
            for m in &l.members {
                index.insert((m.path.as_str(), m.sheet.as_deref(), m.region), m);
            }
        }
        Plans { lock, lock_path: crate::lockfile::lock_path(target_file), held, index }
    }

    /// The lock entry of a member, if the lock lists it.
    pub fn entry(&self, m: &MemberRef) -> Option<&'a Member> {
        self.index.get(&(m.path.as_str(), m.sheet.as_deref(), m.region)).copied()
    }

    pub fn lock(&self) -> Option<&'a Lock> {
        self.lock
    }

    /// This member's plan: its sidecar when the file exists, else the spec
    /// its lock entry names, else `None`. `file` is the member's data file
    /// as the caller resolved it (confined, where that applies); `m` names
    /// it as the lock does, relative to the target.
    ///
    /// A sidecar that exists and is refused is an error, never a fall-back
    /// to the lock: a person's edit must not be passed over in silence.
    pub fn plan_for(&self, file: &Path, m: &MemberRef) -> Result<Option<Plan>> {
        if let Some(p) = sidecar_plan(file, m.sheet.as_deref(), m.region)? {
            return Ok(Some(p));
        }
        let Some(entry) = self.entry(m) else { return Ok(None) };
        self.held_plan(entry)
    }

    /// [`Plans::plan_for`] for a lock entry the caller already holds.
    pub fn plan_for_entry(&self, file: &Path, entry: &Member) -> Result<Option<Plan>> {
        if let Some(p) = sidecar_plan(file, entry.sheet.as_deref(), entry.region)? {
            return Ok(Some(p));
        }
        self.held_plan(entry)
    }

    fn held_plan(&self, entry: &Member) -> Result<Option<Plan>> {
        let Some(id) = &entry.spec else { return Ok(None) };
        let held = match self.held.get(id.as_str()) {
            Some(Ok(h)) => h,
            Some(Err(e)) => bail!("{}: {e}", self.lock_path.display()),
            None => bail!(
                "{}: member {} names plan {} and the lock holds no such plan — run `tdy fit`",
                self.lock_path.display(),
                entry.name(),
                short_id(id)
            ),
        };
        Ok(Some(Plan {
            spec: held.spec.clone(),
            own_notes: entry.notes.clone(),
            method: held.method,
            model: held.model.clone(),
            origin: Origin::Lock { lock: self.lock_path.clone(), id: id.clone() },
            digest: held.digest.clone(),
            state: State::AsLocked { blake3: entry.blake3.clone(), bytes: entry.bytes },
        }))
    }

    /// The digest an acceptance of this member is checked against now: the
    /// sidecar's when the file exists, the lock entry's recomputed identity
    /// otherwise, empty when neither can be read (a missing plan is
    /// reported by whoever loads it).
    pub fn current_digest(&self, file: &Path, entry: &Member) -> String {
        let sc = crate::sidecar::sidecar_path_for(file, entry.sheet.as_deref(), entry.region);
        if let Ok(bytes) = std::fs::read(&sc) {
            return digest_bytes(&bytes);
        }
        match entry.spec.as_deref().and_then(|id| self.held.get(id)) {
            Some(Ok(h)) => h.digest.clone(),
            _ => String::new(),
        }
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("b3:{}", blake3::hash(bytes).to_hex())
}

/// Fingerprint of a member's sidecar file — what an acceptance of a
/// sidecar-held plan is tied to. Empty when there is no sidecar.
pub fn sidecar_digest(file: &Path, sheet: Option<&str>, region: Option<u32>) -> String {
    match std::fs::read(crate::sidecar::sidecar_path_for(file, sheet, region)) {
        Ok(bytes) => digest_bytes(&bytes),
        Err(_) => String::new(),
    }
}

/// The member's sidecar as a [`Plan`], or `None` when it has none. Goes
/// through `sidecar::load_member`, so every check a sidecar has always had
/// (spec version, `validate`, sheet and region agreement, freshness) holds.
fn sidecar_plan(file: &Path, sheet: Option<&str>, region: Option<u32>) -> Result<Option<Plan>> {
    use crate::sidecar::SidecarStatus;
    let path = crate::sidecar::sidecar_path_for(file, sheet, region);
    let (sc, state): (Box<Sidecar>, State) = match crate::sidecar::load_member(file, sheet, region)? {
        SidecarStatus::Absent => return Ok(None),
        SidecarStatus::Fresh(sc) => (sc, State::Fresh),
        SidecarStatus::Stale(sc) => (sc, State::Stale),
    };
    let digest = sidecar_digest(file, sheet, region);
    let sc = *sc;
    Ok(Some(Plan {
        method: sc.provenance.method,
        model: sc.provenance.model,
        spec: Arc::new(sc.spec),
        own_notes: Vec::new(),
        origin: Origin::Sidecar(path),
        digest,
        state,
    }))
}

/// The target whose lock holds a plan for this file, for a caller that was
/// handed a data file and no target (`tdy validate`, `tdy profile`).
///
/// Looks for `*.tdy.lock` beside the file and in up to three directories
/// above it — a target's globs reach into subdirectories, never upwards —
/// and returns the first lock (in name order, nearest directory first) one
/// of whose members *is* this file and names a plan. Each lock is read once.
pub fn find_holding_lock(file: &Path, sheet: Option<&str>, region: Option<u32>) -> Option<(PathBuf, Lock)> {
    let canon = file.canonicalize().ok()?;
    let mut dir = canon.parent().map(Path::to_path_buf);
    // The target as the caller would spell it — beside the file as it was
    // named, or `..` above it — for messages; `canon` is for matching.
    let mut spelled = file.parent().map(Path::to_path_buf).unwrap_or_default();
    for level in 0..4 {
        if level > 0 {
            spelled = spelled.join("..");
        }
        let d = dir?;
        let mut locks: Vec<PathBuf> = std::fs::read_dir(&d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".tdy.lock"))
            .collect();
        locks.sort();
        for lp in locks {
            let target = target_of_lock(&lp);
            let Ok(Some(lock)) = Lock::load(&target) else { continue };
            let Ok(rel) = canon.strip_prefix(&d) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if lock.member(&rel, sheet, region).is_some_and(|m| m.spec.is_some()) {
                let name = target.file_name().map(PathBuf::from).unwrap_or_default();
                return Some((spelled.join(name), lock));
            }
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// A typed member reference — `book.xlsx#Q1`, `report.csv#2` — that no
/// sidecar declares, split by asking which reading a lock beside it holds a
/// plan for ([`find_holding_lock`]). `None` when no reading is held, or
/// when two are (the caller then reports about the name as typed).
pub fn resolve_held(text: &str) -> Option<(PathBuf, Option<String>, Option<u32>)> {
    let held = |m: &MemberRef| {
        let f = Path::new(&m.path);
        (m.sheet.is_some() || m.region.is_some())
            && f.is_file()
            && find_holding_lock(f, m.sheet.as_deref(), m.region).is_some()
    };
    match MemberRef::resolve(text, held) {
        Ok(Some(m)) => Some((PathBuf::from(m.path), m.sheet, m.region)),
        _ => None,
    }
}

/// `items.tdy.lock` -> `items.tdy.sql`
pub fn target_of_lock(lock: &Path) -> PathBuf {
    let name = lock.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let base = name.strip_suffix(".lock").unwrap_or(&name);
    lock.with_file_name(format!("{base}.sql"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::*;

    fn spec() -> ParseSpec {
        ParseSpec {
            extraction: Extraction::Json { lines: false, pointer: None, record: true },
            transforms: vec![],
            columns: vec![ColumnSpec {
                name: "price".into(),
                source: Some("games".into()),
                dtype: DType::Int64,
                nullable: true,
                parse: ValueParsing { na_values: vec!["".into(), "n/a".into()], ..Default::default() },
                pointer: Some("/nh/sellPrice/value".into()),
            }],
            confidence: Some(0.9),
            notes: vec!["`price` <- \"games\"".into()],
        }
    }

    /// Notes are about one fit of one file; everything else is what the plan
    /// does. Identity ignores the first and nothing else.
    #[test]
    fn spec_identity_ignores_notes_and_nothing_else() {
        let a = spec();
        let id = spec_id(&a).unwrap();
        assert!(id.starts_with("b3:") && id.len() == 3 + 64, "{id}");

        let mut notes = a.clone();
        notes.notes = vec!["frame proved by elimination: of 4 candidate frames".into()];
        assert_eq!(spec_id(&notes).unwrap(), id, "notes are not part of a plan's identity");
        notes.notes.clear();
        assert_eq!(spec_id(&notes).unwrap(), id);

        let mut changed: Vec<(&str, ParseSpec)> = Vec::new();
        let mut s = a.clone();
        s.confidence = Some(0.5);
        changed.push(("confidence", s));
        let mut s = a.clone();
        s.columns[0].parse.na_values.pop();
        changed.push(("na_values", s));
        let mut s = a.clone();
        s.columns[0].pointer = Some("/nl/sellPrice/value".into());
        changed.push(("pointer", s));
        let mut s = a.clone();
        s.columns[0].nullable = false;
        changed.push(("nullable", s));
        let mut s = a.clone();
        s.columns[0].name = "nh_price".into();
        changed.push(("name", s));
        let mut s = a.clone();
        s.extraction = Extraction::Json { lines: false, pointer: Some("/items".into()), record: false };
        changed.push(("extraction", s));
        let mut s = a.clone();
        s.transforms.push(Transform::FillDown { columns: vec!["price".into()], direction: Default::default() });
        changed.push(("transforms", s));
        for (what, s) in changed {
            assert_ne!(spec_id(&s).unwrap(), id, "{what} changed and the identity did not");
        }
    }

    #[test]
    fn the_target_of_a_lock_is_its_sql() {
        assert_eq!(target_of_lock(Path::new("d/items.tdy.lock")), PathBuf::from("d/items.tdy.sql"));
    }
}
