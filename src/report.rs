//! Fitting a pile, as data.
//!
//! The orchestration that used to live inside the CLI — plan each member,
//! reuse fresh sidecars, carry acceptances, write the lock only when the
//! whole pile fits — now returns a [`PileReport`], and the CLI's text output
//! is just one renderer of it. That is what makes a `--json` flag, an MCP
//! tool and a TUI three views of one answer instead of three orchestrations
//! that can disagree.
//!
//! The report is complete even when the pile fails: a member that cannot fit
//! appears with its problems structured (`kind`, `column`, `want`, `tried`,
//! the remedy), because for a machine caller the *failure* is the useful
//! output — each problem is an edit it can make.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::Config;
use crate::fit::{FitError, Gap};
use crate::member::MemberRef;
use crate::lockfile::{self, Lock, Member, LOCK_VERSION};
use crate::spec::{Extraction, InferenceMethod, ParseSpec, RowWindow};
use crate::target::Target;

/// The region window a fitted spec's extraction carries, if any — `None`
/// for a whole-file/whole-sheet member and for a workbook region (which
/// carries its ordinal on the `Excel` arm, not a `RowWindow`).
fn spec_window(spec: &ParseSpec) -> Option<RowWindow> {
    match &spec.extraction {
        Extraction::Delimited { region, .. } => *region,
        _ => None,
    }
}

#[derive(Debug, Serialize)]
pub struct PileReport {
    pub target: String,
    pub target_file: String,
    pub declared_columns: usize,
    pub members: Vec<MemberReport>,
    /// Members that fit (including those waiting on review).
    pub fitted: usize,
    pub failed: usize,
    pub needs_review: usize,
    /// Path of the written lock; absent on failure or `--dry-run`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_written: Option<String>,
    pub dry_run: bool,
    /// What the target declares, column by column — so a report is
    /// readable without the `.tdy.sql` beside it, and a pile view can put
    /// each member's binding under the column it supplies.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<TargetColumnReport>,
    /// How the directory disagrees with the lock that existed *before* this
    /// fit (`lockfile::drift`'s messages). Empty when there was no lock, or
    /// when this fit wrote a fresh one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub drift: Vec<String>,
}

/// One declared column of the target, as the report carries it.
#[derive(Debug, Clone, Serialize)]
pub struct TargetColumnReport {
    pub name: String,
    /// The SQL spelling of the declared type (`DECIMAL(14,2)`).
    pub dtype: String,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<String>,
    pub if_missing_null: bool,
}

#[derive(Debug, Serialize)]
pub struct MemberReport {
    /// The member's file, relative to the target.
    pub path: String,
    /// One sheet of that file, when the workbook contributed several
    /// members. `name()` is what the text shows and what `--accept` takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// One of several tables stacked in the file or sheet, counted from 1
    /// in file order — `None` is the whole file or sheet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<u32>,
    /// The raw physical line window this member reads, when it is a
    /// `Delimited` region (`report.csv#2`) rather than a whole file or
    /// sheet — read from the fitted spec, not re-derived, so it always
    /// agrees with what actually executes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<RowWindow>,
    pub status: MemberStatus,
    /// Where the plan came from: heuristic | llm | manual | existing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Declared column -> the file column that supplies it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceBinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    pub accepted: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<Problem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<ProposalReport>,
}

impl MemberReport {
    pub fn name(&self) -> String {
        crate::member::MemberRef { path: self.path.clone(), sheet: self.sheet.clone(), region: self.region }
            .name()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberStatus {
    /// Fits, and (if it needed a judgement) that judgement is accepted.
    Fits,
    /// Fits mechanically, but a judgement in it awaits a human.
    NeedsReview,
    /// One or more declared columns cannot be supplied.
    Gaps,
    /// A hand-written spec that no longer produces the target.
    Contradicts,
    /// Could not be read, framed, or executed.
    Error,
}

#[derive(Debug, Serialize)]
pub struct SourceBinding {
    pub column: String,
    pub source: String,
}

/// One reason a member does not fit, with every field a caller could act on.
#[derive(Debug, Serialize)]
pub struct Problem {
    /// no_candidate | long_form | ambiguous | untypable | ambiguous_separator |
    /// ambiguous_format | collides | ambiguous_frame | contradicts | error
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// The full human-readable report, remedy included.
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub want: Option<String>,
    /// Names that were looked for (no_candidate).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tried: Vec<String>,
    /// The file's own header (no_candidate).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub header: Vec<String>,
    /// Competing readings (ambiguous, ambiguous_frame).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// The sidecar field that settles an ambiguous frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// The file column whose *values* are this declared column (long_form).
    /// Carried structurally, not only in `message`, so a remedy menu built
    /// from `kind` never offers the `matches` binding the prose warns against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_form: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProposalReport {
    pub column: String,
    pub want: String,
    /// (the file's spelling, why it is a candidate)
    pub candidates: Vec<(String, String)>,
    /// Pasteable SQL.
    pub message: String,
}

/// The problems of one `FitError`, as a JSON value — for callers reporting a
/// single file rather than a pile.
pub fn problems_json(e: &FitError) -> serde_json::Value {
    serde_json::to_value(problems_of_error(e)).unwrap_or_default()
}

#[derive(Default)]
pub struct FitOpts<'a> {
    pub dry_run: bool,
    pub accept: &'a [PathBuf],
    pub propose: bool,
    /// Where to send progress while the pile is being fitted. `None` for a
    /// caller that only wants the answer.
    pub progress: Option<crate::progress::Sink>,
    /// When set (the MCP server), every member the target's globs resolve to
    /// must itself resolve under this directory. Globs are joined relative to
    /// the target's directory, so `files = '../…'` reaches outside the served
    /// tree — and a fit *writes* (sidecars beside members, then the lock), so
    /// an unconfined fit would write outside the root, not just read.
    pub root: Option<&'a Path>,
}

fn problem_of_gap(g: &Gap) -> Problem {
    let message = g.message();
    let base = Problem {
        kind: String::new(),
        column: Some(g.column().to_string()),
        message,
        want: None,
        tried: Vec::new(),
        header: Vec::new(),
        choices: Vec::new(),
        field: None,
        long_form: None,
    };
    match g {
        Gap::NoCandidate { want, tried, header, long_form, .. } => Problem {
            kind: if long_form.is_some() { "long_form" } else { "no_candidate" }.into(),
            want: Some(want.clone()),
            tried: tried.clone(),
            header: header.clone(),
            long_form: long_form.clone(),
            ..base
        },
        Gap::Ambiguous { candidates, .. } => Problem {
            kind: "ambiguous".into(),
            choices: candidates.iter().map(|(i, n)| format!("{n} (column {})", i + 1)).collect(),
            ..base
        },
        Gap::Untypable { want, source, .. } => Problem {
            kind: "untypable".into(),
            want: Some(want.clone()),
            choices: vec![source.clone()],
            ..base
        },
        Gap::AmbiguousSeparator { source, .. } => Problem {
            kind: "ambiguous_separator".into(),
            choices: vec![source.clone()],
            ..base
        },
        Gap::AmbiguousFormat { formats, source, .. } => Problem {
            kind: "ambiguous_format".into(),
            choices: formats.clone(),
            field: Some(source.clone()),
            ..base
        },
        Gap::Collides { other, source, .. } => Problem {
            kind: "collides".into(),
            choices: vec![other.clone(), source.clone()],
            ..base
        },
    }
}

fn problems_of_error(e: &FitError) -> Vec<Problem> {
    match e {
        FitError::Gaps(gaps) => gaps.iter().map(problem_of_gap).collect(),
        FitError::AmbiguousFrame { what, field, choices } => vec![Problem {
            kind: "ambiguous_frame".into(),
            column: None,
            message: format!("{e}"),
            want: Some(what.clone()),
            tried: Vec::new(),
            header: Vec::new(),
            choices: choices.clone(),
            field: Some(field.clone()),
            long_form: None,
        }],
        other => vec![Problem {
            kind: "error".into(),
            column: None,
            message: format!("{other}"),
            want: None,
            tried: Vec::new(),
            header: Vec::new(),
            choices: Vec::new(),
            field: None,
            long_form: None,
        }],
    }
}

fn proposals_for(path: &Path, target: &Target, limits: crate::config::Limits) -> Vec<ProposalReport> {
    let Ok(proposals) = crate::fit::propose(path, target, limits) else {
        return Vec::new();
    };
    proposals
        .iter()
        .map(|p| {
            let existing: Vec<String> = target
                .columns
                .iter()
                .find(|c| c.name == p.column)
                .map(|c| {
                    std::iter::once(c.name.clone()).chain(c.matches.iter().cloned()).collect()
                })
                .unwrap_or_default();
            ProposalReport {
                column: p.column.clone(),
                want: p.want.clone(),
                candidates: p.candidates.clone(),
                message: p.message(&existing),
            }
        })
        .collect()
}

/// One member the pile expanded to, before it is fitted: who it is, what
/// the expansion knows about it, and the block window it reads.
#[derive(Debug, Clone)]
pub struct Unit {
    pub member: MemberRef,
    /// Facts about the expansion, copied into the member's spec notes.
    pub notes: Vec<String>,
    /// The block this member is, when its file or sheet was split.
    pub window: Option<RowWindow>,
    /// What the *split* itself asks a person to rule on, before the spec's
    /// own review reasons are added.
    pub review: Option<String>,
    /// The runs the split discarded, structurally rather than as prose — a
    /// member that ends up reusing a *whole-file* spec has to say the same
    /// facts the other way round, since that spec reads those lines.
    pub dropped: Vec<crate::engine::DroppedRun>,
    /// Those of `dropped` shaped like the blocks that were kept: `review`'s
    /// own subject.
    pub shaped: Vec<crate::engine::DroppedRun>,
}

impl Unit {
    /// The notes for a member whose spec reads the whole file although the
    /// split gave this unit a window: the runs are still named, but as lines
    /// this spec *does* read rather than as lines nothing reads.
    fn whole_file_notes(&self) -> Vec<String> {
        let mut ns: Vec<String> =
            self.notes.iter().filter(|n| !is_dropped_note(n)).cloned().collect();
        ns.extend(self.dropped.iter().map(|d| {
            format!(
                "the split found a run of {} line(s) at lines {}–{} outside the proper \
                 block; this spec reads the whole file",
                d.end - d.start,
                d.start + 1,
                d.end
            )
        }));
        ns
    }

    /// And the question that spec is asked instead. The gate does not move:
    /// a whole-file read over a run shaped like the block is a judgement,
    /// just the opposite one from "were those lines data?".
    fn whole_file_reason(&self) -> Option<String> {
        (!self.shaped.is_empty()).then(|| {
            format!(
                "this spec reads the whole file including {} — accept only if that is intended",
                self.shaped
                    .iter()
                    .map(|d| format!("a table-shaped run at lines {}–{}", d.start + 1, d.end))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )
        })
    }
}

/// The members a pile's files resolve to, in lock order: a workbook whose
/// sheets fit becomes one unit per fitting sheet, everything else is one
/// unit for the file; a unit whose file or sheet holds several tables
/// stacked at blank rows then becomes one region unit per table.
/// `exclude`'s exact member references (`file#sheet`, `file#N`,
/// `file#sheet#N`) apply here, after expansion — the only place they can,
/// since before it the members do not exist yet.
///
/// Discovery runs on every fit and is never read back from the previous
/// lock: membership comes from the fit and is what the lock records.
pub fn expand_units(
    rels: &[String],
    target: &Target,
    dir: &Path,
    limits: crate::config::Limits,
) -> Result<Vec<Unit>> {
    let mut sheet_units: Vec<(MemberRef, Vec<String>)> = Vec::new(); // (member, notes)
    for rel in rels {
        let p = dir.join(rel);
        match crate::fit::discover_sheets(&p, target, limits) {
            Ok(Some(d)) if d.fitting.len() >= 2 => {
                let note = expansion_note(&d);
                for sheet in &d.fitting {
                    sheet_units.push((MemberRef::sheet(rel.clone(), sheet.clone()), vec![note.clone()]));
                }
            }
            // One fitting sheet, none, a non-workbook, or an unreadable
            // file: a plain member, and `plan` says what is wrong.
            _ => sheet_units.push((MemberRef::file(rel.clone()), Vec::new())),
        }
    }

    // Regions: ask every unit so far (plain or sheet) whether its file or
    // sheet is really several tables stacked at blank rows. A plain unit
    // that happens to be a single-sheet workbook still needs that sheet
    // named for `regions_of` to read the right thing, even though the
    // member itself stays plain (`book.xlsx#2`, never `book.xlsx#Data#2` —
    // only a sheet that was itself expanded into a member gets `#Sheet#N`).
    let mut units: Vec<Unit> = Vec::new();
    for (member, notes) in sheet_units {
        let p = dir.join(&member.path);
        let regions = match crate::fit::region_read_hint(&p, member.sheet.as_deref(), limits) {
            Some(sheet_hint) => {
                crate::engine::regions_of(&p, sheet_hint.as_deref(), limits).unwrap_or_default()
            }
            // A workbook with several sheets whose member is not tied to
            // one of them: nothing here can say which sheet is meant, so no
            // region discovery runs for it.
            None => Default::default(),
        };
        // Runs the split threw away: with a window applied, no member reads
        // those lines. Every member of this file says which they were, and
        // one shaped like the blocks that were kept waits on a person —
        // "the split was possible" is not "nothing was lost".
        let dropped: Vec<String> = regions.dropped.iter().map(dropped_note).collect();
        let shaped: Vec<crate::engine::DroppedRun> = regions.table_shaped().copied().collect();
        let shaped_reason = (!shaped.is_empty()).then(|| {
            format!(
                "{} — accept only if those lines are not part of this dataset",
                shaped.iter().map(dropped_note).collect::<Vec<_>>().join("; ")
            )
        });
        let n = regions.windows.len();
        match n {
            0 => units.push(Unit {
                member,
                notes,
                window: None,
                review: None,
                dropped: Vec::new(),
                shaped: Vec::new(),
            }),
            1 => {
                let w = regions.windows[0];
                let mut notes = notes;
                notes.push(format!(
                    "one proper block in this file, split at blank rows (lines {}–{})",
                    w.start + 1,
                    w.end
                ));
                notes.extend(dropped);
                units.push(Unit {
                    member,
                    notes,
                    window: Some(w),
                    review: shaped_reason,
                    dropped: regions.dropped.clone(),
                    shaped: shaped.clone(),
                });
            }
            _ => {
                for w in regions.windows {
                    let mut member = member.clone();
                    member.region = Some(w.ordinal);
                    let mut notes = notes.clone();
                    let split =
                        format!("table {} of {n} in this file, split at blank rows", w.ordinal);
                    let review = merge_reason(
                        Some(format!(
                            "{split} — accept only if it is the same kind of table as the others"
                        )),
                        shaped_reason.clone(),
                    );
                    notes.push(split);
                    notes.extend(dropped.iter().cloned());
                    units.push(Unit {
                        member,
                        notes,
                        window: Some(w),
                        review,
                        dropped: regions.dropped.clone(),
                        shaped: shaped.clone(),
                    });
                }
            }
        }
    }

    // `exclude` also takes exact member references, applied after expansion.
    // An entry that removes nothing is a typo — the member it named is still
    // in the dataset and nothing said so — which is an error, not a no-op.
    for x in target.exclude.iter().filter(|x| x.contains('#')) {
        let before = units.len();
        units.retain(|u| *x != u.member.name());
        if units.len() == before {
            anyhow::bail!(
                "exclude {x:?} removes no member of `{}`. Members are named relative to the \
                 target: {}",
                target.name,
                units.iter().take(6).map(|u| format!("{:?}", u.member.name())).collect::<Vec<_>>().join(", ")
            );
        }
    }
    Ok(units)
}

/// How a discarded run reads in a note and in a review reason. The prefix
/// is fixed so `fit_pile` can strip a reused sidecar's copy of it — the
/// dropped runs are a fact about *this* fit's split — and so
/// `render_pile_text` can find it.
fn dropped_note(d: &crate::engine::DroppedRun) -> String {
    format!(
        "a run of {} line(s) at lines {}–{} was not read",
        d.end - d.start,
        d.start + 1,
        d.end
    )
}

/// Does this note name lines nothing read? [`dropped_note`]'s own shape.
fn is_dropped_note(n: &str) -> bool {
    n.starts_with("a run of ") && n.ends_with("was not read")
}

/// Did a hand-edited sidecar get thrown away for this member? The prefix is
/// fixed so `render_pile_text` can find it.
fn is_refusal_note(n: &str) -> bool {
    n.starts_with("sidecar refused: ")
}

/// The notes the CLI shows under a member. Most of a spec's notes are
/// sidecar detail; these are the ones about lines nothing read and about an
/// edit that was discarded, which a person reading the pile has to see.
fn shown_notes(m: &MemberReport) -> impl Iterator<Item = &String> {
    m.notes.iter().filter(|n| {
        is_dropped_note(n) || is_refusal_note(n) || n.starts_with("the split found a run of ")
    })
}

/// A multi-line message as one line — a sidecar's refusal can list several
/// validation problems, and a note is one line under the member.
fn one_line(s: &str) -> String {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

/// A window as a person counts lines: 1-based and inclusive, or "the whole
/// file" when there is none.
fn lines_of(w: Option<RowWindow>) -> String {
    match w {
        Some(w) => format!("lines {}–{}", w.start + 1, w.end),
        None => "the whole file".to_string(),
    }
}

/// What a sheet member's spec records about the expansion it came from.
fn expansion_note(d: &crate::fit::SheetDiscovery) -> String {
    format!(
        "of {} sheets, {} produce the declared table: {}{}",
        d.total,
        d.fitting.len(),
        d.fitting.join(", "),
        if d.rejected.is_empty() { String::new() } else { format!("; {} do not", d.rejected.join(", ")) }
    )
}

/// Rows read per member for the magnitude check: a median over this many
/// is settled long before the file ends, and the read is bounded.
const MAGNITUDE_ROWS: usize = 2000;

/// Append a second review reason to the first, the way the magnitude pass
/// merges its own reason into a member's `review`.
fn merge_reason(base: Option<String>, extra: Option<String>) -> Option<String> {
    match (base, extra) {
        (Some(b), Some(e)) => Some(format!("{b}; {e}")),
        (Some(b), None) => Some(b),
        (None, Some(e)) => Some(e),
        (None, None) => None,
    }
}

/// Was this member's judgement accepted — carried from the previous lock
/// (same bytes, same reason), or named by `--accept` now?
fn carry_over(
    previous: Option<&Lock>,
    unit: &MemberRef,
    blake3: &str,
    review: &Option<String>,
    accepted_now: &[MemberRef],
) -> bool {
    let carried = previous
        .and_then(|l| l.member(&unit.path, unit.sheet.as_deref(), unit.region))
        .filter(|m| m.blake3 == blake3 && m.review == *review)
        .map(|m| m.accepted)
        .unwrap_or(false);
    carried || accepted_now.contains(unit)
}

/// Fit every member the target's globs match; write sidecars and — if all of
/// them fit — the lock. Returns the full report either way: a failed pile is
/// an answer, not an absence of one.
pub async fn fit_pile(
    target_path: &Path,
    cfg: &Config,
    opts: FitOpts<'_>,
) -> Result<PileReport> {
    let limits = cfg.limits;
    let target = Target::load(target_path)?;
    let dir = lockfile::target_dir(target_path);
    // How the directory disagrees with the lock as it stands *now*, before
    // this fit touches it: what a dry run is asked about, and what a failed
    // fit leaves in place.
    let drift = match lockfile::Lock::load(target_path) {
        Ok(Some(lock)) => lockfile::drift(&lock, &target, target_path)
            .map(|d| d.iter().map(|x| x.message()).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let rels = lockfile::resolve(&target, target_path)?;

    if rels.is_empty() {
        anyhow::bail!(
            "no files matched {:?} beside {}",
            target.files,
            target_path.display()
        );
    }

    // Confined mode: every member must resolve under the root before anything
    // is read or written. Checked for the whole pile up front — a fit that
    // sniffed three members and then refused the fourth would already have
    // written three sidecars on the strength of a declaration that reaches
    // outside the served tree.
    if let Some(root) = opts.root {
        for rel in &rels {
            crate::fileio::confine(&dir.join(rel), root).with_context(|| {
                format!(
                    "`{}` resolves member {rel} outside the served root; \
                     a target under --root may only reach files under --root",
                    target.name
                )
            })?;
        }
    }

    // A previous lock's acceptances carry over for entries that have not
    // changed — drift is what expires them, so re-fitting an untouched
    // dataset must not ask the same question twice.
    let previous = Lock::load(target_path)?;
    use crate::member::MemberRef;

    let units = expand_units(&rels, &target, &dir, limits)?;
    if units.is_empty() {
        anyhow::bail!(
            "every member of `{}` is excluded by its own declaration, so there is no dataset \
             to lock. Drop an `exclude` entry, or widen `files`.",
            target.name
        );
    }

    // `--accept` names members the way the report does. A member is
    // identified by its path *relative to the target* (plus an optional
    // sheet), so that is what --accept must name. Matching on the basename
    // accepted the wrong file when two directories held the same name, and
    // could never accept a member in a subdirectory at all.
    let accepted_now: Vec<MemberRef> = opts
        .accept
        .iter()
        .map(|a| {
            let a = a.strip_prefix(&dir).unwrap_or(a);
            let text = a.to_string_lossy().replace('\\', "/");
            match MemberRef::resolve(&text, |m| units.iter().any(|u| &u.member == m)) {
                Ok(Some(m)) => Ok(m),
                Ok(None) => Err(anyhow::anyhow!(
                    "--accept {text:?} is not a member of `{}`. Members are named relative to the \
                     target: {}",
                    target.name,
                    units.iter().take(6).map(|u| format!("{:?}", u.member.name())).collect::<Vec<_>>().join(", ")
                )),
                Err(several) => Err(anyhow::anyhow!(
                    "--accept {text:?} could mean {} — name the file and the sheet unambiguously",
                    MemberRef::names(&several)
                )),
            }
        })
        .collect::<Result<_>>()?;

    let mut reports: Vec<MemberReport> = Vec::new();
    let mut lock_members: Vec<Member> = Vec::new();
    // Every fitted member's spec and file, for the pile-level magnitude
    // check once all of them are known: (index into `reports`, spec, path).
    let mut fitted_specs: Vec<(usize, crate::spec::ParseSpec, PathBuf)> = Vec::new();
    let mut failed = 0usize;
    let mut needs_review = 0usize;

    let total = units.len();
    for (index, u) in units.iter().enumerate() {
        let (unit, unit_notes, window) = (&u.member, &u.notes, &u.window);
        let rel = &unit.path;
        let sheet = unit.sheet.as_deref();
        let region = unit.region;
        let name = unit.name();
        let p = dir.join(rel);
        crate::progress::emit(
            opts.progress.as_ref(),
            crate::progress::Event::MemberStarted {
                path: name.clone(),
                index,
                total,
            },
        );
        // One labelled block per member, so every path out of the work below
        // lands in the same place and `MemberFinished` is emitted exactly
        // once — a UI that missed an event would leave a spinner running
        // forever on a file that had in fact finished.
        'member: {
        // A fresh sidecar that still conforms IS the plan, whoever wrote it.
        // A hand-written one is a human assertion the planner must never
        // overwrite (a contradiction is an error, not a replan); a
        // tool-written one is reused because the acceptance machinery is
        // about *that recorded plan* — replanning on every run would let a
        // nondeterministic model quietly swap the frame out from under a
        // review, and it would re-spend money answering a settled question.
        // Either way it is re-proved: conformance and a dry run, every time.
        let loaded = crate::sidecar::load_member(&p, sheet, region);
        // A sidecar the loader refuses is a person's edit being discarded.
        // Re-planning is the right thing to do — the split's own window is
        // the one fact a sidecar cannot be trusted about — but doing it in
        // silence leaves the member reading exactly as it did before, with
        // nothing to say why the edit had no effect.
        let refused: Option<String> = loaded
            .as_ref()
            .err()
            .map(|e| {
                let m = one_line(&format!("{e:#}"));
                format!("sidecar refused: {}; re-planned", m.trim_end_matches('.'))
            });
        if let Ok(crate::sidecar::SidecarStatus::Fresh(sc)) = loaded {
            let manual = sc.provenance.method == InferenceMethod::Manual;
            let conforming = crate::conform::conforms(&sc.spec, &target).is_ok();
            if manual || conforming {
                let mut spec = sc.spec;
                // The expansion note is a fact about *this* fit's discovery,
                // not about the fit the sidecar was written in: a reused
                // member of a workbook that has since gained or lost a
                // fitting sheet would otherwise report a different sheet
                // count from its own siblings.
                spec.notes.retain(|n| !(n.starts_with("of ") && n.contains(" sheets, ")));
                spec.notes.retain(|n| !(n.starts_with("table ") && n.ends_with("split at blank rows")));
                spec.notes.retain(|n| !n.starts_with("one proper block in this file"));
                spec.notes.retain(|n| !is_dropped_note(n));
                spec.notes.retain(|n| !n.starts_with("the split found a run of "));
                // The split's notes and its review reason are true of a spec
                // that reads one block. A plain member may legitimately reuse
                // a hand-written whole-file spec instead — and then those
                // lines *are* read, so the same facts have to be said the
                // other way round and the person asked the other question.
                // The gate does not move: a whole-file read over a run shaped
                // like the block is still a judgement.
                let whole_file = u.window.is_some()
                    && match &spec.extraction {
                        Extraction::Delimited { region, .. } => region.is_none(),
                        Extraction::Excel { region_ordinal, .. } => region_ordinal.is_none(),
                        _ => true,
                    };
                let (unit_notes, unit_review) = if whole_file {
                    (u.whole_file_notes(), u.whole_file_reason())
                } else {
                    (u.notes.clone(), u.review.clone())
                };
                spec.notes.extend(unit_notes);
                let via = match sc.provenance.method {
                    InferenceMethod::Manual => "manual",
                    InferenceMethod::Llm => "llm",
                    InferenceMethod::Heuristic => "existing",
                };
                // The sidecar names an ordinal (`load_member` proved that);
                // only the split knows which lines that block actually is.
                // A hand-edited window that still names its own ordinal
                // passes every check a sidecar can make on itself, and
                // reusing it makes two members read one block and total a
                // plausible wrong number. Costs no I/O: the true window is
                // already in hand.
                if let Extraction::Delimited { region: declared, .. } = &spec.extraction {
                    if declared.is_some() && declared != window {
                        failed += 1;
                        reports.push(MemberReport {
                            path: rel.clone(),
                            sheet: unit.sheet.clone(),
                            region,
                            window: *declared,
                            status: MemberStatus::Contradicts,
                            via: Some(via.into()),
                            sources: Vec::new(),
                            review: None,
                            accepted: false,
                            notes: Vec::new(),
                            problems: vec![Problem {
                                kind: "contradicts".into(),
                                column: None,
                                message: format!(
                                    "the sidecar reads {}, but the blank-row split puts this \
                                     member's block at {}. A region member's spec must read its \
                                     own block: correct the window, or delete the sidecar and \
                                     re-run `tdy fit`.",
                                    lines_of(*declared),
                                    lines_of(*window),
                                ),
                                want: None,
                                tried: Vec::new(),
                                header: Vec::new(),
                                choices: Vec::new(),
                                field: None,
                                long_form: None,
                            }],
                            proposals: Vec::new(),
                        });
                        break 'member;
                    }
                }
                if let Err(m) = crate::conform::conforms(&spec, &target) {
                    failed += 1;
                    reports.push(MemberReport {
                        path: rel.clone(),
                        sheet: unit.sheet.clone(),
                        region,
                        window: spec_window(&spec),
                        status: MemberStatus::Contradicts,
                        via: Some(via.into()),
                        sources: Vec::new(),
                        review: None,
                        accepted: false,
                        notes: Vec::new(),
                        problems: m
                            .iter()
                            .map(|x| Problem {
                                kind: "contradicts".into(),
                                column: None,
                                message: x.message(),
                                want: None,
                                tried: Vec::new(),
                                header: Vec::new(),
                                choices: Vec::new(),
                                field: None,
                                long_form: None,
                            })
                            .collect(),
                        proposals: Vec::new(),
                    });
                    break 'member;
                }
                if let Err(e) = crate::engine::dry_run(&spec, &p, limits) {
                    failed += 1;
                    reports.push(MemberReport {
                        path: rel.clone(),
                        sheet: unit.sheet.clone(),
                        region,
                        window: spec_window(&spec),
                        status: MemberStatus::Error,
                        via: Some(via.into()),
                        sources: Vec::new(),
                        review: None,
                        accepted: false,
                        notes: Vec::new(),
                        problems: vec![Problem {
                            kind: "error".into(),
                            column: None,
                            message: format!("{e:#}"),
                            want: None,
                            tried: Vec::new(),
                            header: Vec::new(),
                            choices: Vec::new(),
                            field: None,
                            long_form: None,
                        }],
                        proposals: Vec::new(),
                    });
                    break 'member;
                }
                let review = {
                    let mut rs = crate::fit::review_reasons(&spec);
                    // A model-framed plan's judgement is recorded in its
                    // provenance, not in the spec: reconstruct it, or the
                    // review gate would evaporate on the second `tdy fit`.
                    if sc.provenance.method == InferenceMethod::Llm {
                        rs.push(crate::fit::llm_frame_reason(
                            &spec,
                            sc.provenance.model.as_deref().unwrap_or("a model"),
                        ));
                    }
                    (!rs.is_empty()).then(|| rs.join("; "))
                };
                let review = merge_reason(review, unit_review);
                let (blake3, bytes) = crate::sidecar::hash_file(&p)?;
                let carried = previous
                    .as_ref()
                    .and_then(|l| l.member(rel, sheet, region))
                    .filter(|m| m.blake3 == blake3 && m.review == review)
                    .map(|m| m.accepted)
                    .unwrap_or(false);
                let is_accepted = carried || accepted_now.contains(unit);
                let status = match (&review, is_accepted) {
                    (Some(_), false) => {
                        needs_review += 1;
                        MemberStatus::NeedsReview
                    }
                    _ => MemberStatus::Fits,
                };
                reports.push(MemberReport {
                    path: rel.clone(),
                    sheet: unit.sheet.clone(),
                    region,
                    window: spec_window(&spec),
                    status,
                    via: Some(via.into()),
                    sources: spec
                        .columns
                        .iter()
                        .map(|c| SourceBinding {
                            column: c.name.clone(),
                            source: c.source_name().to_string(),
                        })
                        .collect(),
                    review: review.clone(),
                    accepted: is_accepted,
                    notes: spec.notes.clone(),
                    problems: Vec::new(),
                    proposals: Vec::new(),
                });
                fitted_specs.push((reports.len() - 1, spec, p.clone()));
                lock_members.push(Member {
                    path: rel.clone(),
                    sheet: unit.sheet.clone(),
                    region,
                    blake3,
                    bytes,
                    spec_digest: lockfile::spec_digest_for(&p, sheet, region),
                    review,
                    accepted: is_accepted,
                });
                break 'member;
            }
        }
        let planned = match window {
            Some(w) => {
                let region_sheet = crate::fit::region_read_hint(&p, sheet, limits).flatten();
                crate::fit::fit_region(&p, region_sheet.as_deref(), *w, &target, limits).map(|fitted| {
                    crate::fit::Planned { fitted, method: InferenceMethod::Heuristic, model: None }
                })
            }
            None => match sheet {
                Some(s) => {
                    crate::fit::fit_sheet(&p, s, &target, limits).map(|fitted| crate::fit::Planned {
                        fitted,
                        method: InferenceMethod::Heuristic,
                        model: None,
                    })
                }
                None => crate::fit::plan(&p, &target, cfg, opts.progress.as_ref()).await,
            },
        };
        match planned {
            Ok(planned) => {
                let (mut fitted, method, model) = (planned.fitted, planned.method, planned.model);
                fitted.spec.notes.extend(unit_notes.iter().cloned());
                fitted.review = merge_reason(fitted.review, u.review.clone());
                if !opts.dry_run {
                    crate::sidecar::save_member(
                        &p,
                        sheet,
                        region,
                        &fitted.spec,
                        crate::sidecar::ProvenanceInfo {
                            method,
                            model: model.clone(),
                            prompt_version: None,
                            sampled_bytes: None,
                        },
                    )?;
                }
                let (blake3, bytes) = crate::sidecar::hash_file(&p)?;
                let carried = previous
                    .as_ref()
                    .and_then(|l| l.member(rel, sheet, region))
                    .filter(|m| m.blake3 == blake3 && m.review == fitted.review)
                    .map(|m| m.accepted)
                    .unwrap_or(false);
                let is_accepted = carried || accepted_now.contains(unit);
                let status = match (&fitted.review, is_accepted) {
                    (Some(_), false) => {
                        needs_review += 1;
                        MemberStatus::NeedsReview
                    }
                    _ => MemberStatus::Fits,
                };
                reports.push(MemberReport {
                    path: rel.clone(),
                    sheet: unit.sheet.clone(),
                    region,
                    window: spec_window(&fitted.spec),
                    status,
                    via: Some(
                        match method {
                            InferenceMethod::Llm => "llm",
                            _ => "heuristic",
                        }
                        .into(),
                    ),
                    sources: fitted
                        .spec
                        .columns
                        .iter()
                        .map(|c| SourceBinding {
                            column: c.name.clone(),
                            source: c.source_name().to_string(),
                        })
                        .collect(),
                    review: fitted.review.clone(),
                    accepted: is_accepted,
                    notes: {
                        // On the report, not in the spec: the refusal is a
                        // fact about *this* fit, and the sidecar just written
                        // is the plan that replaced the refused one.
                        let mut ns = fitted.spec.notes.clone();
                        ns.extend(refused.clone());
                        ns
                    },
                    problems: Vec::new(),
                    proposals: Vec::new(),
                });
                fitted_specs.push((reports.len() - 1, fitted.spec, p.clone()));
                lock_members.push(Member {
                    path: rel.clone(),
                    sheet: unit.sheet.clone(),
                    region,
                    blake3,
                    bytes,
                    spec_digest: lockfile::spec_digest_for(&p, sheet, region),
                    review: fitted.review.clone(),
                    accepted: is_accepted,
                });
            }
            Err(e) => {
                failed += 1;
                let status = match &e {
                    FitError::Gaps(_) => MemberStatus::Gaps,
                    _ => MemberStatus::Error,
                };
                let proposals = if opts.propose && status == MemberStatus::Gaps {
                    proposals_for(&p, &target, limits)
                } else {
                    Vec::new()
                };
                reports.push(MemberReport {
                    path: rel.clone(),
                    sheet: unit.sheet.clone(),
                    region,
                    window: None,
                    status,
                    via: None,
                    sources: Vec::new(),
                    review: None,
                    accepted: false,
                    notes: refused.into_iter().collect(),
                    problems: problems_of_error(&e),
                    proposals,
                });
            }
        }
        }
        crate::progress::emit(
            opts.progress.as_ref(),
            crate::progress::Event::MemberFinished {
                path: name.clone(),
                index,
                total,
                status: reports.last().map(|r| r.status).unwrap_or(MemberStatus::Error),
            },
        );
    }

    // The pile-level question no single file can answer: is one member's
    // money in a different unit? Every fitted member's typical value per
    // numeric column, against the pile's; an outlier gets a review reason
    // and waits, like a shift does, and the acceptance carries over on the
    // same terms — the reason is deterministic, so an unchanged pile asks
    // once.
    if fitted_specs.len() >= crate::magnitude::MIN_MEMBERS {
        let mut medians: Vec<crate::magnitude::Medians> = Vec::with_capacity(fitted_specs.len());
        for (_, spec, p) in &fitted_specs {
            medians.push(match crate::engine::preview(spec, p, limits, MAGNITUDE_ROWS) {
                Ok(batch) => crate::magnitude::medians(&batch),
                Err(_) => crate::magnitude::Medians::new(),
            });
        }
        for o in crate::magnitude::outliers(&medians, crate::magnitude::THRESHOLD) {
            let (ri, _, p) = &fitted_specs[o.member];
            let report = &mut reports[*ri];
            let unit = MemberRef { path: report.path.clone(), sheet: report.sheet.clone(), region: report.region };
            let review = match &report.review {
                Some(r) => Some(format!("{r}; {}", o.reason())),
                None => Some(o.reason()),
            };
            let (blake3, _) = crate::sidecar::hash_file(p)?;
            let is_accepted = carry_over(previous.as_ref(), &unit, &blake3, &review, &accepted_now);
            let lock_member = lock_members
                .iter_mut()
                .find(|m| m.path == unit.path && m.sheet == unit.sheet && m.region == unit.region)
                .expect("a fitted member has a lock entry");
            lock_member.review = review.clone();
            lock_member.accepted = is_accepted;
            report.review = review;
            report.accepted = is_accepted;
            report.status = if is_accepted { MemberStatus::Fits } else { MemberStatus::NeedsReview };
        }
        needs_review = reports.iter().filter(|r| r.status == MemberStatus::NeedsReview).count();
    }

    let fitted = lock_members.len();
    let mut lock_written = None;
    if failed == 0 && !opts.dry_run {
        // No partial lock. A dataset missing a month is the failure this
        // whole design refuses, and writing one here would make it the
        // default outcome of a bad afternoon.
        let lock = Lock {
            lock_version: LOCK_VERSION,
            target: target.name.clone(),
            target_hash: lockfile::target_hash(&target),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: crate::sidecar::now_rfc3339(),
            members: lock_members,
        };
        let p = lock.save(target_path)?;
        lock_written = Some(p.display().to_string());
    }

    Ok(PileReport {
        target: target.name.clone(),
        target_file: target_path.display().to_string(),
        declared_columns: target.columns.len(),
        members: reports,
        fitted,
        failed,
        needs_review,
        columns: target
            .columns
            .iter()
            .map(|c| TargetColumnReport {
                name: c.name.clone(),
                dtype: crate::fit::render(&c.dtype),
                nullable: c.nullable,
                matches: c.matches.clone(),
                if_missing_null: c.if_missing_null,
            })
            .collect(),
        // A fresh lock supersedes whatever the old one disagreed with.
        drift: if lock_written.is_some() { Vec::new() } else { drift },
        lock_written,
        dry_run: opts.dry_run,
    })
}

/// The CLI's rendering of a pile report — line-compatible with what
/// `tdy fit` printed before the report existed, because scripts and tests
/// read it.
pub fn render_pile_text(r: &PileReport) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    // A pile with sheet members counts members; a plain pile keeps saying
    // "file(s)", which is what its readers and tests have always seen.
    let unit = if r.members.iter().any(|m| m.sheet.is_some() || m.region.is_some()) {
        "member(s)"
    } else {
        "file(s)"
    };
    line(format!(
        "{}: {} {unit} match, {} declared column(s)\n",
        r.target,
        r.members.len(),
        r.declared_columns
    ));

    for m in &r.members {
        let label = match m.via.as_deref() {
            Some("manual") => "  (hand-written spec)",
            Some("llm") => "  (model-framed spec)",
            Some("existing") => "  (existing spec)",
            _ => "",
        };
        match m.status {
            MemberStatus::Fits | MemberStatus::NeedsReview => {
                let sources: Vec<String> = m
                    .sources
                    .iter()
                    .map(|s| format!("{}<-{:?}", s.column, s.source))
                    .collect();
                let word = match (m.review.is_some(), m.accepted) {
                    (true, true) => "accepted",
                    (true, false) => "REVIEW  ",
                    (false, _) => "fits    ",
                };
                line(format!("  {:<24} {word}{label}  {}", m.name(), sources.join("  ")));
                // Lines the split discarded are not sidecar trivia: with a
                // window applied nothing reads them, so a member that fits
                // says so out loud, accepted or not.
                for n in shown_notes(m) {
                    line(format!("      note: {n}"));
                }
                if let (Some(rv), false) = (&m.review, m.accepted) {
                    line(format!("      REVIEW: {rv}"));
                    line(
                        "      tdy does not accept a value-changing step on its own judgement."
                            .into(),
                    );
                    line(format!("      Accept:  tdy fit {} --accept {}", r.target_file, m.name()));
                }
            }
            MemberStatus::Contradicts => {
                line(format!("  {:<24} CONTRADICTS{label}", m.name()));
                for pr in &m.problems {
                    for l in pr.message.lines() {
                        line(format!("      {l}"));
                    }
                }
            }
            MemberStatus::Gaps => {
                line(format!("  {:<24} GAP", m.name()));
                for n in shown_notes(m) {
                    line(format!("      note: {n}"));
                }
                for pr in &m.problems {
                    for l in pr.message.lines() {
                        line(format!("      {l}"));
                    }
                }
                for p in &m.proposals {
                    line(format!("    `{}` ({}):", p.column, p.want));
                    for l in p.message.lines() {
                        line(format!("      {l}"));
                    }
                }
            }
            MemberStatus::Error => {
                line(format!("  {:<24} ERROR{label}", m.name()));
                for n in shown_notes(m) {
                    line(format!("      note: {n}"));
                }
                for pr in &m.problems {
                    for l in pr.message.lines() {
                        line(format!("      {l}"));
                    }
                }
            }
        }
    }

    line(format!(
        "\n{} of {} {unit} fit `{}`.",
        r.fitted,
        r.members.len(),
        r.target
    ));
    if r.needs_review > 0 {
        line(format!(
            "{} member(s) need a human before they can join. \
             Nothing is wrong with them mechanically — that is the point.",
            r.needs_review
        ));
    }
    if let Some(p) = &r.lock_written {
        line(format!("wrote {p}"));
        line(format!("\nQuery it:  tdy query \"SELECT * FROM dataset('{}')\"", r.target_file));
    }
    if r.dry_run && r.failed == 0 {
        line("--dry-run: no sidecars and no lock written.".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pile(exclude: &str) -> (tempfile::TempDir, Target) {
        let d = tempfile::TempDir::new().unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_two_fit.xlsx"),
            d.path().join("2025.xlsx"),
        )
        .unwrap();
        let t = d.path().join("monat.tdy.sql");
        std::fs::write(
            &t,
            format!(
                "CREATE TABLE monat (\n  month DATE NOT NULL OPTIONS(matches = 'Datum'),\n  \
                 region TEXT NOT NULL OPTIONS(matches = 'Region'),\n  \
                 amount DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')\n) \
                 WITH (files = '*.xlsx', date_order = 'dmy'{exclude});\n"
            ),
        )
        .unwrap();
        let target = Target::load(&t).unwrap();
        (d, target)
    }

    /// Expansion is a function of the files and the declaration, so it can be
    /// asked directly: a two-sheet workbook is two members, both carrying the
    /// note that says where they came from.
    #[test]
    fn expansion_turns_a_two_sheet_workbook_into_two_members() {
        let (d, target) = pile("");
        let units =
            expand_units(&["2025.xlsx".to_string()], &target, d.path(), Default::default()).unwrap();
        let names: Vec<String> = units.iter().map(|u| u.member.name()).collect();
        assert_eq!(names, vec!["2025.xlsx#Q1", "2025.xlsx#Q2"]);
        assert!(units[0].notes[0].contains("of 2 sheets, 2 produce"), "{:?}", units[0].notes);
    }

    #[test]
    fn a_member_exclude_removes_that_member_and_a_miss_is_an_error() {
        let (d, target) = pile(", exclude = '2025.xlsx#Q2'");
        let units =
            expand_units(&["2025.xlsx".to_string()], &target, d.path(), Default::default()).unwrap();
        assert_eq!(units.iter().map(|u| u.member.name()).collect::<Vec<_>>(), ["2025.xlsx#Q1"]);

        let (d, target) = pile(", exclude = '2025.xlsx#Q9'");
        let err = expand_units(&["2025.xlsx".to_string()], &target, d.path(), Default::default())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("2025.xlsx#Q9") && msg.contains("2025.xlsx#Q1"), "{msg}");
    }
}
