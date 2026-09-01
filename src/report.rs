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

use anyhow::Result;
use serde::Serialize;

use crate::config::Config;
use crate::fit::{FitError, Gap};
use crate::lockfile::{self, Lock, Member, LOCK_VERSION};
use crate::spec::InferenceMethod;
use crate::target::Target;

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
}

#[derive(Debug, Serialize)]
pub struct MemberReport {
    pub path: String,
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
    /// no_candidate | ambiguous | untypable | ambiguous_separator |
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
    };
    match g {
        Gap::NoCandidate { want, tried, header, .. } => Problem {
            kind: "no_candidate".into(),
            want: Some(want.clone()),
            tried: tried.clone(),
            header: header.clone(),
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
    let rels = lockfile::resolve(&target, target_path)?;

    if rels.is_empty() {
        anyhow::bail!(
            "no files matched {:?} beside {}",
            target.files,
            target_path.display()
        );
    }

    // A previous lock's acceptances carry over for entries that have not
    // changed — drift is what expires them, so re-fitting an untouched
    // dataset must not ask the same question twice.
    let previous = Lock::load(target_path)?;
    // A member is identified by its path *relative to the target*, so that is
    // what --accept must name. Matching on the basename accepted the wrong
    // file when two directories held the same name, and could never accept a
    // member in a subdirectory at all.
    let accepted_now: Vec<String> = opts
        .accept
        .iter()
        .map(|a| {
            let a = a.strip_prefix(&dir).unwrap_or(a);
            a.to_string_lossy().replace('\\', "/")
        })
        .collect();
    for a in &accepted_now {
        if !rels.contains(a) {
            anyhow::bail!(
                "--accept {a:?} is not a member of `{}`. Members are named relative to the \
                 target: {}",
                target.name,
                rels.iter().take(6).map(|r| format!("{r:?}")).collect::<Vec<_>>().join(", ")
            );
        }
    }

    let mut reports: Vec<MemberReport> = Vec::new();
    let mut lock_members: Vec<Member> = Vec::new();
    let mut failed = 0usize;
    let mut needs_review = 0usize;

    let total = rels.len();
    for (index, rel) in rels.iter().enumerate() {
        let p = dir.join(rel);
        crate::progress::emit(
            opts.progress.as_ref(),
            crate::progress::Event::MemberStarted {
                path: rel.clone(),
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
        if let Ok(crate::sidecar::SidecarStatus::Fresh(sc)) = crate::sidecar::load(&p) {
            let manual = sc.provenance.method == InferenceMethod::Manual;
            let conforming = crate::conform::conforms(&sc.spec, &target).is_ok();
            if manual || conforming {
                let spec = sc.spec;
                let via = match sc.provenance.method {
                    InferenceMethod::Manual => "manual",
                    InferenceMethod::Llm => "llm",
                    InferenceMethod::Heuristic => "existing",
                };
                if let Err(m) = crate::conform::conforms(&spec, &target) {
                    failed += 1;
                    reports.push(MemberReport {
                        path: rel.clone(),
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
                let (blake3, bytes) = crate::sidecar::hash_file(&p)?;
                let carried = previous
                    .as_ref()
                    .and_then(|l| l.member(rel))
                    .filter(|m| m.blake3 == blake3 && m.review == review)
                    .map(|m| m.accepted)
                    .unwrap_or(false);
                let is_accepted = carried || accepted_now.iter().any(|a| a == rel);
                let status = match (&review, is_accepted) {
                    (Some(_), false) => {
                        needs_review += 1;
                        MemberStatus::NeedsReview
                    }
                    _ => MemberStatus::Fits,
                };
                reports.push(MemberReport {
                    path: rel.clone(),
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
                lock_members.push(Member {
                    path: rel.clone(),
                    blake3,
                    bytes,
                    spec_digest: lockfile::spec_digest(&p),
                    review,
                    accepted: is_accepted,
                });
                break 'member;
            }
        }
        match crate::fit::plan(&p, &target, cfg, opts.progress.as_ref()).await {
            Ok(planned) => {
                let (fitted, method, model) = (planned.fitted, planned.method, planned.model);
                if !opts.dry_run {
                    crate::sidecar::save(
                        &p,
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
                    .and_then(|l| l.member(rel))
                    .filter(|m| m.blake3 == blake3 && m.review == fitted.review)
                    .map(|m| m.accepted)
                    .unwrap_or(false);
                let is_accepted = carried || accepted_now.iter().any(|a| a == rel);
                let status = match (&fitted.review, is_accepted) {
                    (Some(_), false) => {
                        needs_review += 1;
                        MemberStatus::NeedsReview
                    }
                    _ => MemberStatus::Fits,
                };
                reports.push(MemberReport {
                    path: rel.clone(),
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
                    notes: fitted.spec.notes.clone(),
                    problems: Vec::new(),
                    proposals: Vec::new(),
                });
                lock_members.push(Member {
                    path: rel.clone(),
                    blake3,
                    bytes,
                    spec_digest: lockfile::spec_digest(&p),
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
                    status,
                    via: None,
                    sources: Vec::new(),
                    review: None,
                    accepted: false,
                    notes: Vec::new(),
                    problems: problems_of_error(&e),
                    proposals,
                });
            }
        }
        }
        crate::progress::emit(
            opts.progress.as_ref(),
            crate::progress::Event::MemberFinished {
                path: rel.clone(),
                index,
                total,
                status: reports.last().map(|r| r.status).unwrap_or(MemberStatus::Error),
            },
        );
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
    line(format!(
        "{}: {} file(s) match, {} declared column(s)\n",
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
                line(format!("  {:<24} {word}{label}  {}", m.path, sources.join("  ")));
                if let (Some(rv), false) = (&m.review, m.accepted) {
                    line(format!("      REVIEW: {rv}"));
                    line(
                        "      tdy does not accept a value-changing step on its own judgement."
                            .into(),
                    );
                    line(format!("      Accept:  tdy fit {} --accept {}", r.target_file, m.path));
                }
            }
            MemberStatus::Contradicts => {
                line(format!("  {:<24} CONTRADICTS{label}", m.path));
                for pr in &m.problems {
                    for l in pr.message.lines() {
                        line(format!("      {l}"));
                    }
                }
            }
            MemberStatus::Gaps => {
                line(format!("  {:<24} GAP", m.path));
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
                line(format!("  {:<24} ERROR{label}", m.path));
                for pr in &m.problems {
                    for l in pr.message.lines() {
                        line(format!("      {l}"));
                    }
                }
            }
        }
    }

    line(format!(
        "\n{} of {} file(s) fit `{}`.",
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
