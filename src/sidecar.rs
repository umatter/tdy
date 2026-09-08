//! Sidecar handling: `<file>.tdy.toml` next to the raw file.
//!
//! Freshness = the blake3 of the raw file matches the fingerprint recorded
//! in the sidecar. A stale sidecar is never silently used: callers either
//! re-sniff (default) or hard-error (`--frozen`).
//!
//! Two properties matter beyond that:
//!
//! - **A sidecar on disk is untrusted input.** It may have been hand-edited
//!   (that is an advertised workflow) or written by an older version. It is
//!   therefore validated on load, not just deserialized — the executor has
//!   invariants that `serde` cannot express, and a spec that violates them
//!   used to reach the extractors and panic.
//! - **A half-written sidecar is worse than none**, because its header would
//!   still be trusted on the next run. Writes go through a temp file and a
//!   rename.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::fileio;
use crate::spec::{
    InferenceMethod, ParseSpec, Provenance, Sidecar, SourceFingerprint, SPEC_FORMAT_VERSION,
};

/// `<file>.tdy.toml`, `<file>#<sheet>.tdy.toml` for one sheet of a
/// workbook, or `<file>[#<sheet>]#<N>.tdy.toml` for one region within it:
/// the selector is part of the sidecar's *name*, so the browser's companion
/// folding needs nothing, and the single-file tools (`validate`, `--stamp`,
/// `check --against`, `.edit`) take a `file#sheet#N` reference and find it
/// through [`resolve_ref`].
pub fn sidecar_path_for(file: &Path, sheet: Option<&str>, region: Option<u32>) -> PathBuf {
    let mut name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(s) = sheet {
        name.push('#');
        name.push_str(s);
    }
    if let Some(r) = region {
        name.push('#');
        name.push_str(&r.to_string());
    }
    name.push_str(".tdy.toml");
    file.with_file_name(name)
}

pub fn sidecar_path(file: &Path) -> PathBuf {
    sidecar_path_for(file, None, None)
}

/// Split a typed reference — `file`, `file#sheet`, or `file[#sheet]#N` —
/// into the data file, the sheet, and the region it names, by asking the
/// filesystem rather than by a rule (a `#` is legal in a file name and in a
/// sheet name alike).
///
/// The plain path wins when it is a file; otherwise the split whose data
/// file exists *and* has a sidecar beside it that declares itself to be
/// about that member — see [`declares_member`]. Existence alone is not
/// enough: `report.csv#2.tdy.toml` is equally the sidecar of sheet `2`, of
/// region 2, and of a file literally called `report.csv#2`, so a check that
/// only asked whether the path exists saw two readings of every region
/// member and refused it as ambiguous. Text that names nothing comes back
/// unchanged, so the caller reports about the name that was typed.
pub fn resolve_ref(text: &Path) -> Result<(PathBuf, Option<String>, Option<u32>)> {
    if text.is_file() {
        return Ok((text.to_path_buf(), None, None));
    }
    let s = text.to_string_lossy().into_owned();
    match crate::member::MemberRef::resolve(&s, |m| {
        let f = Path::new(&m.path);
        f.is_file() && declares_member(f, m.sheet.as_deref(), m.region)
    }) {
        Ok(Some(m)) => Ok((PathBuf::from(m.path), m.sheet, m.region)),
        Ok(None) => Ok((text.to_path_buf(), None, None)),
        Err(several) => bail!(
            "{s} could mean {} — name the file and the sheet unambiguously",
            crate::member::MemberRef::names(&several)
        ),
    }
}

/// Is there a sidecar beside `file` that says it is the spec for *this*
/// member — that sheet and that region, as its own fingerprint records
/// them?
///
/// The sidecar's name cannot answer this: `report.csv#2.tdy.toml` is the
/// path of the region-2 reading and of the sheet-`"2"` reading alike, and a
/// resolver that took existence for an answer therefore found two members
/// wherever there was one. The sidecar's `source` block says which reading
/// it is, and one file holds one declaration, so at most one candidate can
/// be true. Anything unreadable or unparseable is not a declaration and
/// answers `false`; the caller's ordinary "no such member" follows.
pub fn declares_member(file: &Path, sheet: Option<&str>, region: Option<u32>) -> bool {
    let p = sidecar_path_for(file, sheet, region);
    let Ok(text) = std::fs::read_to_string(&p) else { return false };
    match toml::from_str::<Sidecar>(&text) {
        Ok(sc) => sc.source.sheet.as_deref() == sheet && sc.source.region == region,
        Err(_) => false,
    }
}

/// The sheets of `file` that have a sidecar beside it, in name order.
/// A workbook expanded into sheet members has no plain sidecar, and
/// reporting "no sidecar" about it while its members sit in the same
/// directory is a wrong answer.
///
/// A tail that parses fully as a *positive* integer is a region of the plain
/// file (`report.csv#2.tdy.toml`), not a sheet named `"2"` — the same
/// exclusion [`region_sidecars`] applies in reverse to tell a sheet name
/// from a region ordinal, and with the same floor: ordinals count from 1, so
/// a workbook whose sheet is literally named `0` still has an ordinary sheet
/// sidecar and must still be listed. A tail ending in `#<N>` is a *region of
/// a sheet* (`book.xlsx#Q1#2.tdy.toml`): the ordinal is stripped before what
/// remains is treated as the sheet name, or a sheet split into regions would
/// be reported as a sheet literally called `"Q1#2"` — a name that appears
/// once per region sidecar, when it is really one sheet.
pub fn sheet_sidecars(file: &Path) -> Vec<String> {
    let Some(name) = file.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    let dir = match file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let prefix = format!("{name}#");
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let rest = n.strip_prefix(&prefix)?.strip_suffix(".tdy.toml")?;
            if rest.is_empty() {
                return None;
            }
            // Strip a trailing region ordinal, if there is one, before
            // judging whether what remains is a sheet name or itself a bare
            // ordinal (a region of the plain file).
            let sheet = match rest.rsplit_once('#') {
                Some((head, tail)) if !head.is_empty() && is_ordinal(tail) => head,
                _ => rest,
            };
            if is_ordinal(sheet) {
                return None;
            }
            Some(sheet.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Is this name segment a region ordinal? Ordinals count from 1, so `0` is
/// a sheet name like any other.
fn is_ordinal(s: &str) -> bool {
    s.parse::<u32>().is_ok_and(|n| n >= 1)
}

/// The region ordinals of `file` (or of one of its sheets, when `sheet` is
/// given) that have a sidecar beside it, ascending. A file split into
/// several stacked tables has one sidecar per region
/// (`<file>[#<sheet>]#<N>.tdy.toml`) and no plain one, mirroring
/// [`sheet_sidecars`] for the region dimension.
pub fn region_sidecars(file: &Path, sheet: Option<&str>) -> Vec<u32> {
    let Some(name) = file.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    let dir = match file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let prefix = match sheet {
        Some(s) => format!("{name}#{s}#"),
        None => format!("{name}#"),
    };
    let mut out: Vec<u32> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let rest = n.strip_prefix(&prefix)?.strip_suffix(".tdy.toml")?;
            is_ordinal(rest).then(|| rest.parse().ok())?
        })
        .collect();
    out.sort_unstable();
    out
}

/// blake3 of the file's contents plus its length, streamed.
pub fn hash_file(file: &Path) -> Result<(String, u64)> {
    fileio::hash_file(file).with_context(|| format!("cannot fingerprint {}", file.display()))
}

#[derive(Debug)]
pub enum SidecarStatus {
    Fresh(Box<Sidecar>),
    Stale(Box<Sidecar>),
    Absent,
}

impl SidecarStatus {
    pub fn fresh_spec(self) -> Option<ParseSpec> {
        match self {
            SidecarStatus::Fresh(sc) => Some(sc.spec),
            _ => None,
        }
    }
}

pub fn load_member(file: &Path, sheet: Option<&str>, region: Option<u32>) -> Result<SidecarStatus> {
    let sc_path = sidecar_path_for(file, sheet, region);
    if !sc_path.exists() {
        return Ok(SidecarStatus::Absent);
    }
    let text = std::fs::read_to_string(&sc_path)
        .with_context(|| format!("cannot read sidecar {}", sc_path.display()))?;
    let sidecar: Sidecar = toml::from_str(&text)
        .with_context(|| format!("sidecar {} is not a valid spec", sc_path.display()))?;
    if sidecar.spec_version != SPEC_FORMAT_VERSION {
        bail!(
            "sidecar {} has spec_version {}, this build understands {}",
            sc_path.display(),
            sidecar.spec_version,
            SPEC_FORMAT_VERSION
        );
    }
    // A sidecar is editable by hand, so it is untrusted input: check the
    // cross-field invariants before anything downstream can rely on them.
    if let Err(errs) = sidecar.spec.validate() {
        bail!(
            "sidecar {} is not a valid parsing spec:\n- {}",
            sc_path.display(),
            errs.join("\n- ")
        );
    }
    // A sheet member's sidecar is trusted for exactly one sheet, and both
    // places that name it are hand-editable: the fingerprint's `sheet` and
    // the spec's own `sheet_name`. If either disagrees with the sheet being
    // loaded, `dataset()` would read some other sheet under this member's
    // label and total a plausible wrong number.
    if let Some(s) = sheet {
        let stated = sidecar.source.sheet.as_deref();
        let framed = match &sidecar.spec.extraction {
            crate::spec::Extraction::Excel { sheet_name, .. } => sheet_name.as_deref(),
            _ => None,
        };
        if stated != Some(s) || framed != Some(s) {
            bail!(
                "sidecar {} is the spec for sheet {:?}, but it says source.sheet = {} and \
                 reads sheet {}. A sheet member's sidecar must be about its own sheet: \
                 correct it, or re-run `tdy fit`.",
                sc_path.display(),
                s,
                named(stated),
                named(framed)
            );
        }
    }
    // A region member's sidecar is trusted for exactly one region, and both
    // places that name it are hand-editable: the fingerprint's `region` and
    // the ordinal the spec's own window (or `range`) carries. If either
    // disagrees with the region being loaded, `dataset()` would read some
    // other block under this member's label — two members totalling one
    // block twice, which is the plausible wrong number this refuses.
    if let Some(r) = region {
        let framed = match &sidecar.spec.extraction {
            crate::spec::Extraction::Delimited { region, .. } => region.map(|w| w.ordinal),
            crate::spec::Extraction::Excel { region_ordinal, .. } => *region_ordinal,
            _ => None,
        };
        if sidecar.source.region != Some(r) || framed != Some(r) {
            bail!(
                "sidecar {} is the spec for region {r}, but it says source.region = {} and \
                 reads the block with ordinal {}. A region member's sidecar must be about \
                 its own region: correct it, or re-run `tdy fit`.",
                sc_path.display(),
                numbered(sidecar.source.region),
                numbered(framed)
            );
        }
    }
    let (hash, _) = hash_file(file)?;
    if hash == sidecar.source.blake3 {
        Ok(SidecarStatus::Fresh(Box::new(sidecar)))
    } else {
        Ok(SidecarStatus::Stale(Box::new(sidecar)))
    }
}

/// A region ordinal as it reads in a message, or "(none)".
fn numbered(n: Option<u32>) -> String {
    n.map(|n| n.to_string()).unwrap_or_else(|| "(none)".into())
}

/// A sheet name as it reads in a message: quoted, or "(none)".
fn named(sheet: Option<&str>) -> String {
    match sheet {
        Some(s) => format!("{s:?}"),
        None => "(none)".to_string(),
    }
}

pub fn load(file: &Path) -> Result<SidecarStatus> {
    load_member(file, None, None)
}

pub struct ProvenanceInfo {
    pub method: InferenceMethod,
    pub model: Option<String>,
    pub prompt_version: Option<String>,
    pub sampled_bytes: Option<u64>,
}

pub fn save_member(
    file: &Path,
    sheet: Option<&str>,
    region: Option<u32>,
    spec: &ParseSpec,
    prov: ProvenanceInfo,
) -> Result<PathBuf> {
    let (hash, bytes) = hash_file(file)?;
    let sidecar = Sidecar {
        spec_version: SPEC_FORMAT_VERSION,
        source: SourceFingerprint {
            path: file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.display().to_string()),
            blake3: hash,
            bytes,
            sheet: sheet.map(str::to_string),
            region,
            compressed: fileio::compression_kind(file)?.map(|c| c.name().to_string()),
        },
        provenance: Provenance {
            method: prov.method,
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: now_rfc3339(),
            model: prov.model,
            prompt_version: prov.prompt_version,
            sampled_bytes: prov.sampled_bytes,
        },
        spec: spec.clone(),
    };
    let sc_path = sidecar_path_for(file, sheet, region);
    let text = toml::to_string_pretty(&sidecar).context("serializing sidecar")?;
    fileio::atomic_write(&sc_path, &text)?;
    Ok(sc_path)
}

pub fn save(file: &Path, spec: &ParseSpec, prov: ProvenanceInfo) -> Result<PathBuf> {
    save_member(file, None, None, spec, prov)
}

/// Re-fingerprint an existing sidecar against the current file, keeping the
/// spec exactly as written.
///
/// This is what makes "edit the sidecar by hand" a real workflow: after
/// changing the data file (or writing a spec from scratch) there has to be a
/// way to say "yes, this spec is for this file" without re-running inference
/// and losing the edit.
pub fn stamp(file: &Path, method: InferenceMethod) -> Result<PathBuf> {
    stamp_member(file, None, None, method)
}

/// [`stamp`] for one member: a sheet or region member's sidecar is a
/// different file beside the same data, and it is stamped exactly the same
/// way.
pub fn stamp_member(
    file: &Path,
    sheet: Option<&str>,
    region: Option<u32>,
    method: InferenceMethod,
) -> Result<PathBuf> {
    let sc_path = sidecar_path_for(file, sheet, region);
    if !sc_path.exists() {
        bail!(
            "no sidecar at {} to stamp; run `tdy sniff {}` to create one",
            sc_path.display(),
            file.display()
        );
    }
    let text = std::fs::read_to_string(&sc_path)
        .with_context(|| format!("cannot read sidecar {}", sc_path.display()))?;
    let mut sidecar: Sidecar = toml::from_str(&text)
        .with_context(|| format!("sidecar {} is not a valid spec", sc_path.display()))?;
    if let Err(errs) = sidecar.spec.validate() {
        bail!(
            "refusing to stamp an invalid spec in {}:\n- {}",
            sc_path.display(),
            errs.join("\n- ")
        );
    }
    let (hash, bytes) = hash_file(file)?;
    if method == InferenceMethod::Manual {
        // `confidence` is a machine's self-assessment of a guess it no longer
        // owns, and `notes` are that guess's caveats — "no column alignment
        // was found" is actively misleading on a spec that now defines the
        // columns by hand. Stamping is a human taking authorship.
        sidecar.spec.confidence = None;
        sidecar.spec.notes.clear();
    }
    sidecar.spec_version = SPEC_FORMAT_VERSION;
    sidecar.source.blake3 = hash;
    sidecar.source.bytes = bytes;
    sidecar.provenance.method = method;
    sidecar.provenance.tool_version = env!("CARGO_PKG_VERSION").to_string();
    sidecar.provenance.created_at = now_rfc3339();
    let out = toml::to_string_pretty(&sidecar).context("serializing sidecar")?;
    fileio::atomic_write(&sc_path, &out)?;
    Ok(sc_path)
}

/// RFC 3339 UTC timestamp without pulling in a clock-formatting dependency
/// beyond chrono, which we already have.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::*;

    fn minimal() -> ParseSpec {
        ParseSpec {
            extraction: Extraction::Json { lines: true, pointer: None },
            transforms: vec![],
            columns: vec![ColumnSpec {
                name: "a".into(),
                source: None,
                dtype: DType::Utf8,
                nullable: true,
                parse: ValueParsing::default(),
                pointer: None,
            }],
            confidence: Some(0.42),
            notes: vec!["a heuristic doubt".into()],
        }
    }

    #[test]
    fn round_trip_and_freshness() {
        let d = tempfile::TempDir::new().unwrap();
        let f = d.path().join("a.ndjson");
        std::fs::write(&f, "{\"a\":1}\n").unwrap();
        save(
            &f,
            &minimal(),
            ProvenanceInfo {
                method: InferenceMethod::Manual,
                model: None,
                prompt_version: None,
                sampled_bytes: None,
            },
        )
        .unwrap();
        assert!(matches!(load(&f).unwrap(), SidecarStatus::Fresh(_)));
        std::fs::write(&f, "{\"a\":2}\n").unwrap();
        assert!(matches!(load(&f).unwrap(), SidecarStatus::Stale(_)));
    }

    #[test]
    fn stamping_makes_a_hand_edited_sidecar_fresh_again() {
        let d = tempfile::TempDir::new().unwrap();
        let f = d.path().join("a.ndjson");
        std::fs::write(&f, "{\"a\":1}\n").unwrap();
        save(
            &f,
            &minimal(),
            ProvenanceInfo {
                method: InferenceMethod::Heuristic,
                model: None,
                prompt_version: None,
                sampled_bytes: None,
            },
        )
        .unwrap();
        std::fs::write(&f, "{\"a\":2}\n{\"a\":3}\n").unwrap();
        assert!(matches!(load(&f).unwrap(), SidecarStatus::Stale(_)));
        stamp(&f, InferenceMethod::Manual).unwrap();
        match load(&f).unwrap() {
            SidecarStatus::Fresh(sc) => {
                assert_eq!(sc.provenance.method, InferenceMethod::Manual);
                // A hand-owned spec does not carry the machine's old doubts.
                assert!(sc.spec.confidence.is_none());
                assert!(sc.spec.notes.is_empty());
            }
            _ => panic!("expected fresh after stamping"),
        }
    }

    #[test]
    fn an_invalid_hand_edited_sidecar_is_rejected_on_load() {
        let d = tempfile::TempDir::new().unwrap();
        let f = d.path().join("a.txt");
        std::fs::write(&f, "hello\n").unwrap();
        let (hash, bytes) = hash_file(&f).unwrap();
        // end < start is exactly the kind of edit that used to reach the
        // extractor and panic on a slice.
        let toml_text = format!(
            r#"spec_version = 1
[source]
path = "a.txt"
blake3 = "{hash}"
bytes = {bytes}
[provenance]
method = "manual"
tool_version = "0.1.0"
created_at = "2026-01-01T00:00:00Z"
[spec.extraction]
format = "fixed_width"
[[spec.extraction.fields]]
name = "a"
start = 10
end = 2
[[spec.columns]]
name = "a"
dtype = {{ type = "utf8" }}
"#
        );
        std::fs::write(sidecar_path(&f), toml_text).unwrap();
        let err = match load(&f) {
            Err(e) => e,
            Ok(_) => panic!("an invalid spec must not load"),
        };
        assert!(format!("{err:#}").contains("not a valid parsing spec"));
    }

    #[test]
    fn stamping_refuses_a_spec_that_is_invalid() {
        let d = tempfile::TempDir::new().unwrap();
        let f = d.path().join("a.txt");
        std::fs::write(&f, "hello\n").unwrap();
        std::fs::write(
            sidecar_path(&f),
            r#"spec_version = 1
[source]
path = "a.txt"
blake3 = "0"
bytes = 0
[provenance]
method = "manual"
tool_version = "0.1.0"
created_at = "2026-01-01T00:00:00Z"
[spec.extraction]
format = "delimited"
delimiter = ","
[[spec.columns]]
name = "a"
dtype = { type = "decimal", precision = 0, scale = 0 }
"#,
        )
        .unwrap();
        assert!(stamp(&f, InferenceMethod::Manual).is_err());
    }

    #[test]
    fn a_sheet_sidecar_sits_beside_its_workbook_under_the_sheets_name() {
        let f = Path::new("/data/2025.xlsx");
        assert_eq!(sidecar_path_for(f, None, None), PathBuf::from("/data/2025.xlsx.tdy.toml"));
        assert_eq!(sidecar_path_for(f, Some("Q1"), None), PathBuf::from("/data/2025.xlsx#Q1.tdy.toml"));
        assert_eq!(sidecar_path(f), sidecar_path_for(f, None, None));
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
        let p = save_member(&book, Some("Q1"), None, &sheet_spec("Q1"), prov()).unwrap();
        assert!(p.ends_with("book.xlsx#Q1.tdy.toml"), "{}", p.display());
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("sheet = \"Q1\""), "the fingerprint names the sheet:\n{text}");

        match load_member(&book, Some("Q1"), None).unwrap() {
            SidecarStatus::Fresh(sc) => assert_eq!(sc.source.sheet.as_deref(), Some("Q1")),
            other => panic!("expected Fresh, got {other:?}"),
        }
        // The plain sidecar is a different file, and absent.
        assert!(matches!(load(&book).unwrap(), SidecarStatus::Absent));
        // A second sheet's sidecar is another file again.
        save_member(&book, Some("Q2"), None, &sheet_spec("Q2"), prov()).unwrap();
        assert!(matches!(load_member(&book, Some("Q2"), None).unwrap(), SidecarStatus::Fresh(_)));
        assert!(matches!(load_member(&book, Some("Q1"), None).unwrap(), SidecarStatus::Fresh(_)));
    }

    /// A sheet member's sidecar is trusted for exactly one sheet. A hand
    /// edit of `sheet_name` would otherwise make `dataset()` read Q1 twice,
    /// label one of them Q2, and total the wrong number in silence.
    #[test]
    fn a_sheet_sidecar_that_reads_another_sheet_is_refused() {
        let d = tempfile::TempDir::new().unwrap();
        let book = d.path().join("book.xlsx");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sheet_frames_two_fit.xlsx"),
            &book,
        )
        .unwrap();
        let p = save_member(
            &book,
            Some("Q2"),
            None,
            &sheet_spec("Q2"),
            ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None },
        )
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, text.replace("sheet_name = \"Q2\"", "sheet_name = \"Q1\"")).unwrap();
        let err = match load_member(&book, Some("Q2"), None) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("a sidecar that reads another sheet must not load"),
        };
        assert!(err.contains("Q1") && err.contains("Q2"), "{err}");
    }

    #[test]
    fn a_region_sidecar_sits_under_its_ordinal_and_states_it() {
        let f = Path::new("/d/report.csv");
        assert_eq!(sidecar_path_for(f, None, Some(2)), PathBuf::from("/d/report.csv#2.tdy.toml"));
        assert_eq!(sidecar_path_for(Path::new("/d/book.xlsx"), Some("Q1"), Some(2)), PathBuf::from("/d/book.xlsx#Q1#2.tdy.toml"));
        assert_eq!(sidecar_path_for(f, None, None), sidecar_path(f));

        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("report.csv");
        std::fs::write(&p, "a;b\n1;2\n\na;b\n3;4\n").unwrap();
        let mut spec = sheet_spec("unused");   // any valid spec; make it delimited with a window
        spec.extraction = Extraction::Delimited { delimiter: ';', quote: None, escape: None, encoding: None, comment: None, ragged: Default::default(), region: Some(RowWindow { start: 3, end: 5, ordinal: 2 }) };
        let prov = || ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None };
        let sc = save_member(&p, None, Some(2), &spec, prov()).unwrap();
        assert!(std::fs::read_to_string(&sc).unwrap().contains("region = 2"));
        assert!(matches!(load_member(&p, None, Some(2)).unwrap(), SidecarStatus::Fresh(_)));
        assert!(matches!(load_member(&p, None, None).unwrap(), SidecarStatus::Absent));
        assert_eq!(region_sidecars(&p, None), vec![2]);
        // A region sidecar that says it is region 1 is not region 2's.
        let text = std::fs::read_to_string(&sc).unwrap().replace("region = 2", "region = 1");
        std::fs::write(&sc, text).unwrap();
        let e = format!("{:#}", load_member(&p, None, Some(2)).unwrap_err());
        assert!(e.contains("region") && e.contains('2') && e.contains('1'), "{e}");
    }

    /// `report.csv#2.tdy.toml` is a region of the plain file, not a sheet
    /// called `"2"` — `sheet_sidecars` must not claim it, or a freshly
    /// fitted region file would be checked against a sheet that does not
    /// exist and reported stale.
    #[test]
    fn sheet_sidecars_ignores_a_region_only_sidecar() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("report.csv");
        std::fs::write(&p, "a;b\n1;2\n").unwrap();
        let mut spec = sheet_spec("unused");
        spec.extraction = Extraction::Delimited { delimiter: ';', quote: None, escape: None, encoding: None, comment: None, ragged: Default::default(), region: None };
        let prov = || ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None };
        save_member(&p, None, Some(2), &spec, prov()).unwrap();
        assert!(sheet_sidecars(&p).is_empty(), "a region ordinal is not a sheet name");
        assert_eq!(region_sidecars(&p, None), vec![2]);
    }

    /// A sheet split into stacked regions has one sidecar per region and no
    /// plain sheet sidecar — `book.xlsx#Q1#1.tdy.toml`,
    /// `book.xlsx#Q1#2.tdy.toml` — and `sheet_sidecars` must report the
    /// sheet once, not report a sheet literally called `"Q1#2"` for every
    /// region sidecar it finds.
    #[test]
    fn sheet_sidecars_collapses_a_sheets_region_sidecars_to_one_sheet_name() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("book.xlsx");
        std::fs::write(&p, b"not read as a workbook here; only the bytes are hashed").unwrap();
        let mut spec = sheet_spec("Q1");
        spec.extraction =
            Extraction::Excel { sheet_name: Some("Q1".into()), sheet_index: None, range: Some("A1:B1".into()), region_ordinal: None };
        let prov = || ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None };
        save_member(&p, Some("Q1"), Some(1), &spec, prov()).unwrap();
        save_member(&p, Some("Q1"), Some(2), &spec, prov()).unwrap();
        assert_eq!(sheet_sidecars(&p), vec!["Q1".to_string()]);
    }

    /// Region ordinals count from 1, so `0` is not one: a workbook with a
    /// sheet literally named `0` has an ordinary sheet sidecar
    /// (`book.xlsx#0.tdy.toml`), and excluding it as "a region" would
    /// report the workbook as having no sheet specs at all.
    #[test]
    fn a_sheet_literally_named_zero_is_a_sheet_not_a_region() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("book.xlsx");
        std::fs::write(&p, b"not read as a workbook here; only the bytes are hashed").unwrap();
        let prov = || ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None };
        save_member(&p, Some("0"), None, &sheet_spec("0"), prov()).unwrap();
        assert_eq!(sheet_sidecars(&p), vec!["0".to_string()]);
        assert!(region_sidecars(&p, None).is_empty(), "0 is not an ordinal");

        // And the same for a sheet named `0` that was itself split into
        // regions: `book.xlsx#0#1.tdy.toml` is sheet "0", region 1.
        let q = d.path().join("other.xlsx");
        std::fs::write(&q, b"not read as a workbook here; only the bytes are hashed").unwrap();
        let mut spec = sheet_spec("0");
        spec.extraction = Extraction::Excel { sheet_name: Some("0".into()), sheet_index: None, range: Some("A1:B1".into()), region_ordinal: Some(1) };
        save_member(&q, Some("0"), Some(1), &spec, prov()).unwrap();
        assert_eq!(sheet_sidecars(&q), vec!["0".to_string()]);
    }

    fn sheet_spec(sheet: &str) -> ParseSpec {
        ParseSpec {
            extraction: Extraction::Excel { sheet_name: Some(sheet.into()), sheet_index: None, range: None, region_ordinal: None },
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
}
