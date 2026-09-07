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

pub fn load_member(file: &Path, sheet: Option<&str>) -> Result<SidecarStatus> {
    let sc_path = sidecar_path_for(file, sheet);
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
    let (hash, _) = hash_file(file)?;
    if hash == sidecar.source.blake3 {
        Ok(SidecarStatus::Fresh(Box::new(sidecar)))
    } else {
        Ok(SidecarStatus::Stale(Box::new(sidecar)))
    }
}

/// A sheet name as it reads in a message: quoted, or "(none)".
fn named(sheet: Option<&str>) -> String {
    match sheet {
        Some(s) => format!("{s:?}"),
        None => "(none)".to_string(),
    }
}

pub fn load(file: &Path) -> Result<SidecarStatus> {
    load_member(file, None)
}

pub struct ProvenanceInfo {
    pub method: InferenceMethod,
    pub model: Option<String>,
    pub prompt_version: Option<String>,
    pub sampled_bytes: Option<u64>,
}

pub fn save_member(file: &Path, sheet: Option<&str>, spec: &ParseSpec, prov: ProvenanceInfo) -> Result<PathBuf> {
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
    let sc_path = sidecar_path_for(file, sheet);
    let text = toml::to_string_pretty(&sidecar).context("serializing sidecar")?;
    fileio::atomic_write(&sc_path, &text)?;
    Ok(sc_path)
}

pub fn save(file: &Path, spec: &ParseSpec, prov: ProvenanceInfo) -> Result<PathBuf> {
    save_member(file, None, spec, prov)
}

/// Re-fingerprint an existing sidecar against the current file, keeping the
/// spec exactly as written.
///
/// This is what makes "edit the sidecar by hand" a real workflow: after
/// changing the data file (or writing a spec from scratch) there has to be a
/// way to say "yes, this spec is for this file" without re-running inference
/// and losing the edit.
pub fn stamp(file: &Path, method: InferenceMethod) -> Result<PathBuf> {
    let sc_path = sidecar_path(file);
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
            &sheet_spec("Q2"),
            ProvenanceInfo { method: InferenceMethod::Manual, model: None, prompt_version: None, sampled_bytes: None },
        )
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, text.replace("sheet_name = \"Q2\"", "sheet_name = \"Q1\"")).unwrap();
        let err = match load_member(&book, Some("Q2")) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("a sidecar that reads another sheet must not load"),
        };
        assert!(err.contains("Q1") && err.contains("Q2"), "{err}");
    }

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
}
