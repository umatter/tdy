# The console (slice 1) — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `tdy` with no arguments opens a `tdy>` console where SQL is the default language and every CLI subcommand is a dot-command, backed by a library dispatcher (`tdy::console`) that returns typed outcomes — the layer slice 2's workbench will be built on.

**Architecture:** A pure parser (`console::parse`) turns a line into a `Command`; a `Session` runs it against the existing non-printing library functions and returns an `Outcome { echo, text, payload, ok }` whose `text` is byte-identical to what the CLI subcommand prints (the CLI arms are refactored to call the same text-producing functions, so the promise is structural). A small raw-mode line editor and a REPL loop wrap the session for a TTY; piped stdin makes it a batch runner. `evidence` moves from `tdy-tui` into the library because `.accept`'s first step needs it.

**Tech Stack:** Rust ≥ 1.88, DataFusion 46, tokio, `crossterm` (new direct dependency of `tdy`, already in the workspace via `tdy-tui`), `dirs` 5 (already a dependency) for the history file.

**Spec:** `docs/design/2026-09-01-console-and-workbench.md` — sections 3, 4, 5, 8, 9, 10 and slice 1 of 11.

## Global Constraints

- Rust ≥ 1.88; `cargo test --workspace --lib --tests` must stay green after every task (446 tests today; plain `cargo test` has a spurious doc-test failure on this machine).
- CI runs `cargo clippy --all-targets -- -D warnings` and clippy is **not installed locally**: avoid `too_many_arguments` (group into a struct), unused imports, needless borrows.
- **Nothing in `src/console/` or `src/commands.rs` may write to stdout or stderr.** Text is returned, never printed. (`mcp.rs` follows the same rule; stdout is protocol there.)
- **Every path a console command takes is confined to the session root with `fileio::confine`** at the point of use; a refusal is an ordinary error `Outcome`.
- **No `.accept` for more than one member, no flag that skips its first step.**
- **The one rule:** tdy never silently produces a wrong value. Where this plan deviates from the spec for that reason (query context is not cached across commands — Task 8) the deviation is recorded in the spec.
- Commit after every task with the repo's message style: a first line that says what changed and why, the body for the non-obvious, ending with the `Co-Authored-By` and `Claude-Session` trailers used in this repo's history.
- Tests never need a network or a model: every console test runs with `backend = none` (`Config::default()` after `config::load` with `Overrides { backend: Some("none".into()), .. }`, or the `--no-llm` flag).

---

## File structure

| file | responsibility |
|---|---|
| `src/console/mod.rs` | `Session`, `Outcome`, `Payload`, the per-command `run_*` methods; re-exports `parse::{parse, Command, ParseError}` |
| `src/console/parse.rs` | tokenizer, `Command`, `ParseError`, `parse` — pure, no I/O |
| `src/console/line.rs` | `LineEditor`: a state machine (`KeyEvent` in, `Edit` out) for the raw-mode prompt, with in-memory history |
| `src/console/repl.rs` | `run_interactive` (TTY loop over `LineEditor`) and `run_batch` (piped stdin); history file load/append |
| `src/commands.rs` | the CLI subcommands as functions that **return text**: `sniff_text`, `validate_text`, `check_text`, `fit_one_text`, `describe_dtype`; `main.rs` prints what they return |
| `src/evidence.rs` | moved from `tdy-tui/src/evidence.rs`, unchanged in behaviour |
| `src/lockfile.rs` | gains `pub fn expand_glob(dir, pattern)` reusing the existing matcher |
| `src/provider.rs` | `sniff_command`/`validate_command` become one-line wrappers over `commands::*_text` |
| `src/main.rs` | `command: Option<Command>`, new `Command::Console`, the no-argument dispatch |
| `tests/console.rs` | `parse` table tests, `Session` end to end over a tempdir copy of `testdata/drifting_exports`, the same-text assertions against the binary |
| `tests/repl.rs` | the binary with piped stdin |
| `README.md`, `CLAUDE.md` | a "Console" section; the quick start uses the console; CLAUDE.md's module notes |

---

### Task 1: `lockfile::expand_glob` — the console's glob expansion

The console has no shell in front of it, so `.draft 2025-*.csv` must expand its own globs. `lockfile.rs` already has the matcher (`matches_glob`, iterative, hang-proof) and the `dir/name` split; this exposes them for a single pattern against a directory.

**Files:**
- Modify: `src/lockfile.rs` (add after `resolve`, ~line 350)
- Test: `src/lockfile.rs` `mod tests`

**Interfaces:**
- Produces: `pub fn expand_glob(dir: &Path, pattern: &str) -> Result<Vec<PathBuf>>` — files only, sorted, joined onto `dir`; skips `*.tdy.toml` and `*.tdy.lock`; a pattern without `*`/`?` returns `vec![dir.join(pattern)]` whether or not it exists (so a missing literal file is reported by the command that opens it, not swallowed as "no match"); a glob matching nothing returns an empty vec.

- [ ] **Step 1: Write the failing tests** in `src/lockfile.rs` `mod tests`:

```rust
    #[test]
    fn expand_glob_matches_files_sorted_and_skips_companions() {
        let d = tempfile::tempdir().unwrap();
        for n in ["b.csv", "a.csv", "a.csv.tdy.toml", "t.tdy.lock", "x.txt"] {
            std::fs::write(d.path().join(n), "").unwrap();
        }
        std::fs::create_dir(d.path().join("sub.csv")).unwrap(); // a dir, not a file
        let got = expand_glob(d.path(), "*.csv").unwrap();
        let names: Vec<String> =
            got.iter().map(|p| p.file_name().unwrap().to_string_lossy().into()).collect();
        assert_eq!(names, ["a.csv", "b.csv"]);
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
```

`tempfile` is already a dev-dependency.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib lockfile::tests::expand_glob`
Expected: compile error, `expand_glob` not found.

- [ ] **Step 3: Implement** after `resolve` in `src/lockfile.rs`:

```rust
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
```

Add `use anyhow::Context;` if the file does not already import it (it uses `Result`; check the top of the file).

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib lockfile::tests`
Expected: all pass, including the pre-existing glob tests.

- [ ] **Step 5: Commit**

```bash
git add src/lockfile.rs
git commit -m "lockfile::expand_glob: one pattern against one directory, for a caller with no shell"
```

---

### Task 2: `console::parse` — the grammar, pure

**Files:**
- Create: `src/console/mod.rs` (skeleton: `pub mod parse; pub use parse::{parse, Command, ParseError};`)
- Create: `src/console/parse.rs`
- Modify: `src/lib.rs` (add `pub mod console;` in alphabetical position and a module-map line)
- Test: `src/console/parse.rs` `mod tests`

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Sniff { file: String, quick: bool, force: bool, no_llm: bool, hint: Option<String> },
    Validate { file: String, stamp: bool },
    Draft { files: Vec<String>, to: Option<String> },
    Fit { target: String, file: Option<String>, dry_run: bool, propose: bool },
    Check { target: String, against: Vec<String> },
    Accept { target: String, member: String },
    /// `.output` alone routes back to the screen (file = None).
    Output { file: Option<String>, format: Option<String>, force: bool },
    Schema,
    ConfigInit,
    Ls { dir: Option<String> },
    Cd { dir: String },
    Show { file: String },
    Edit { file: String },
    Help { command: Option<String> },
    Quit,
    /// Any line not starting with `.`. Multi-line assembly is the Session's job.
    Sql(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Unknown(String),                                  // `.frobnicate`
    Missing { command: &'static str, what: &'static str },  // `.fit` → command "fit", what "TARGET"
    Unexpected { command: &'static str, token: String },    // `.schema foo`, `.sniff a b`
    UnknownFlag { command: &'static str, flag: String },
    FlagNeedsValue { command: &'static str, flag: String },
    UnterminatedQuote,
}
impl std::fmt::Display for ParseError { /* one sentence each, see step 3 */ }

pub fn tokenize(s: &str) -> Result<Vec<String>, ParseError>;
pub fn parse(line: &str) -> Result<Command, ParseError>;
```

`Missing` is a distinct variant on purpose: slice 2's workbench fills in the browser's selection when it sees `Missing { what: "FILE" }` and re-dispatches the completed line.

- [ ] **Step 1: Write the failing tests** in `src/console/parse.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Command {
        parse(s).unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    #[test]
    fn tokenizer_handles_quotes_and_whitespace() {
        assert_eq!(tokenize("a  b\t'c d' \"e f\"").unwrap(), ["a", "b", "c d", "e f"]);
        assert_eq!(tokenize("").unwrap(), Vec::<String>::new());
        assert_eq!(tokenize("'unterminated"), Err(ParseError::UnterminatedQuote));
    }

    #[test]
    fn sql_is_anything_not_starting_with_a_dot() {
        assert_eq!(p("SELECT 1;"), Command::Sql("SELECT 1;".into()));
        assert_eq!(p("  select 1"), Command::Sql("select 1".into()));
    }

    #[test]
    fn sniff_with_every_flag() {
        assert_eq!(
            p(".sniff 2025-01.csv --quick --force --no-llm --hint 'nginx log'"),
            Command::Sniff {
                file: "2025-01.csv".into(),
                quick: true,
                force: true,
                no_llm: true,
                hint: Some("nginx log".into()),
            }
        );
        assert_eq!(
            parse(".sniff"),
            Err(ParseError::Missing { command: "sniff", what: "FILE" })
        );
        assert_eq!(
            parse(".sniff a.csv b.csv"),
            Err(ParseError::Unexpected { command: "sniff", token: "b.csv".into() })
        );
        assert_eq!(
            parse(".sniff a.csv --hint"),
            Err(ParseError::FlagNeedsValue { command: "sniff", flag: "--hint".into() })
        );
        assert_eq!(
            parse(".sniff a.csv --bogus"),
            Err(ParseError::UnknownFlag { command: "sniff", flag: "--bogus".into() })
        );
    }

    #[test]
    fn validate_draft_fit_check() {
        assert_eq!(p(".validate a.csv --stamp"), Command::Validate { file: "a.csv".into(), stamp: true });
        assert_eq!(
            p(".draft 2025-*.csv 2025-*.xlsx --to sales.tdy.sql"),
            Command::Draft { files: vec!["2025-*.csv".into(), "2025-*.xlsx".into()], to: Some("sales.tdy.sql".into()) }
        );
        assert_eq!(parse(".draft"), Err(ParseError::Missing { command: "draft", what: "FILES" }));
        assert_eq!(
            p(".fit sales.tdy.sql 2025-07.csv --dry-run --propose"),
            Command::Fit { target: "sales.tdy.sql".into(), file: Some("2025-07.csv".into()), dry_run: true, propose: true }
        );
        assert_eq!(p(".fit t.tdy.sql"), Command::Fit { target: "t.tdy.sql".into(), file: None, dry_run: false, propose: false });
        assert_eq!(
            p(".check t.tdy.sql --against a.csv --against b.csv"),
            Command::Check { target: "t.tdy.sql".into(), against: vec!["a.csv".into(), "b.csv".into()] }
        );
    }

    #[test]
    fn accept_needs_exactly_target_and_member() {
        assert_eq!(p(".accept t.tdy.sql 2025-07.csv"), Command::Accept { target: "t.tdy.sql".into(), member: "2025-07.csv".into() });
        assert_eq!(parse(".accept t.tdy.sql"), Err(ParseError::Missing { command: "accept", what: "MEMBER" }));
        assert_eq!(
            parse(".accept t.tdy.sql a.csv b.csv"),
            Err(ParseError::Unexpected { command: "accept", token: "b.csv".into() })
        );
        assert_eq!(parse(".accept t.tdy.sql a.csv --yes"), Err(ParseError::UnknownFlag { command: "accept", flag: "--yes".into() }));
    }

    #[test]
    fn output_schema_config_and_navigation() {
        assert_eq!(p(".output"), Command::Output { file: None, format: None, force: false });
        assert_eq!(
            p(".output out.parquet --format parquet --force"),
            Command::Output { file: Some("out.parquet".into()), format: Some("parquet".into()), force: true }
        );
        assert_eq!(p(".schema"), Command::Schema);
        assert_eq!(parse(".schema x"), Err(ParseError::Unexpected { command: "schema", token: "x".into() }));
        assert_eq!(p(".config init"), Command::ConfigInit);
        assert_eq!(parse(".config"), Err(ParseError::Missing { command: "config", what: "init" }));
        assert_eq!(p(".ls"), Command::Ls { dir: None });
        assert_eq!(p(".ls sub"), Command::Ls { dir: Some("sub".into()) });
        assert_eq!(p(".cd .."), Command::Cd { dir: "..".into() });
        assert_eq!(p(".show a.csv"), Command::Show { file: "a.csv".into() });
        assert_eq!(p(".edit t.tdy.sql"), Command::Edit { file: "t.tdy.sql".into() });
        assert_eq!(p(".help"), Command::Help { command: None });
        assert_eq!(p(".help fit"), Command::Help { command: Some("fit".into()) });
        assert_eq!(p(".quit"), Command::Quit);
        assert_eq!(p(".exit"), Command::Quit);
        assert_eq!(parse(".nope"), Err(ParseError::Unknown("nope".into())));
    }

    #[test]
    fn errors_read_as_sentences() {
        assert_eq!(ParseError::Missing { command: "fit", what: "TARGET" }.to_string(), "`.fit` needs a TARGET");
        assert_eq!(ParseError::Unknown("nope".into()).to_string(), "unknown command `.nope` — `.help` lists them");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib console::parse`
Expected: compile errors (module missing).

- [ ] **Step 3: Implement `src/console/parse.rs`**

```rust
//! The console grammar: a line in, a [`Command`] out. Pure — no I/O, no
//! state — so every line shape is a table test.
//!
//! A line starting with `.` is a dot-command, anything else is SQL; that is
//! sqlite's rule and it needs no explaining. Dot-command arguments are
//! tokenised like a shell (whitespace, single or double quotes) because
//! paths have spaces in them, but nothing else of the shell is imitated:
//! globs are expanded by the session, which knows the working directory.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command { /* as in Interfaces */ }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError { /* as in Interfaces */ }

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Unknown(c) => write!(f, "unknown command `.{c}` — `.help` lists them"),
            ParseError::Missing { command, what } => write!(f, "`.{command}` needs a {what}"),
            ParseError::Unexpected { command, token } => {
                write!(f, "`.{command}` does not take `{token}`")
            }
            ParseError::UnknownFlag { command, flag } => {
                write!(f, "`.{command}` has no flag `{flag}`")
            }
            ParseError::FlagNeedsValue { command, flag } => {
                write!(f, "`.{command} {flag}` needs a value")
            }
            ParseError::UnterminatedQuote => write!(f, "unterminated quote"),
        }
    }
}
impl std::error::Error for ParseError {}

pub fn tokenize(s: &str) -> Result<Vec<String>, ParseError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_tok = false;
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                in_tok = true;
            }
            None if ch.is_whitespace() => {
                if in_tok {
                    out.push(std::mem::take(&mut cur));
                    in_tok = false;
                }
            }
            None => {
                cur.push(ch);
                in_tok = true;
            }
        }
    }
    if quote.is_some() {
        return Err(ParseError::UnterminatedQuote);
    }
    if in_tok {
        out.push(cur);
    }
    Ok(out)
}

/// Splits a dot-command's tokens into positionals and flags, validating
/// flags against what the command accepts.
struct Args {
    command: &'static str,
    positional: Vec<String>,
    switches: Vec<String>,          // flags without a value, e.g. "--quick"
    values: Vec<(String, String)>,  // flags with a value, e.g. ("--hint", "nginx")
}

impl Args {
    fn collect(
        command: &'static str,
        tokens: &[String],
        switch_names: &[&str],
        value_names: &[&str],
    ) -> Result<Args, ParseError> {
        let mut a = Args { command, positional: vec![], switches: vec![], values: vec![] };
        let mut it = tokens.iter();
        while let Some(t) = it.next() {
            if let Some(flag) = t.strip_prefix("--").map(|_| t.as_str()) {
                if switch_names.contains(&flag) {
                    a.switches.push(flag.to_string());
                } else if value_names.contains(&flag) {
                    let v = it.next().ok_or_else(|| ParseError::FlagNeedsValue {
                        command,
                        flag: flag.to_string(),
                    })?;
                    a.values.push((flag.to_string(), v.clone()));
                } else {
                    return Err(ParseError::UnknownFlag { command, flag: flag.to_string() });
                }
            } else {
                a.positional.push(t.clone());
            }
        }
        Ok(a)
    }
    fn on(&self, flag: &str) -> bool {
        self.switches.iter().any(|s| s == flag)
    }
    fn value(&self, flag: &str) -> Option<String> {
        self.values.iter().find(|(f, _)| f == flag).map(|(_, v)| v.clone())
    }
    fn values_of(&self, flag: &str) -> Vec<String> {
        self.values.iter().filter(|(f, _)| f == flag).map(|(_, v)| v.clone()).collect()
    }
    /// Exactly `n` positionals, the names used for the Missing error.
    fn exactly(&self, names: &[&'static str]) -> Result<(), ParseError> {
        self.at_least(names)?;
        if let Some(extra) = self.positional.get(names.len()) {
            return Err(ParseError::Unexpected { command: self.command, token: extra.clone() });
        }
        Ok(())
    }
    fn at_least(&self, names: &[&'static str]) -> Result<(), ParseError> {
        if self.positional.len() < names.len() {
            return Err(ParseError::Missing {
                command: self.command,
                what: names[self.positional.len()],
            });
        }
        Ok(())
    }
    fn at_most(&self, n: usize) -> Result<(), ParseError> {
        if let Some(extra) = self.positional.get(n) {
            return Err(ParseError::Unexpected { command: self.command, token: extra.clone() });
        }
        Ok(())
    }
}

pub fn parse(line: &str) -> Result<Command, ParseError> {
    let line = line.trim();
    let Some(rest) = line.strip_prefix('.') else {
        return Ok(Command::Sql(line.to_string()));
    };
    let tokens = tokenize(rest)?;
    let (name, args) = match tokens.split_first() {
        Some((n, a)) => (n.as_str(), a),
        None => return Err(ParseError::Unknown(String::new())),
    };
    Ok(match name {
        "sniff" => {
            let a = Args::collect("sniff", args, &["--quick", "--force", "--no-llm"], &["--hint"])?;
            a.exactly(&["FILE"])?;
            Command::Sniff {
                file: a.positional[0].clone(),
                quick: a.on("--quick"),
                force: a.on("--force"),
                no_llm: a.on("--no-llm"),
                hint: a.value("--hint"),
            }
        }
        "validate" => {
            let a = Args::collect("validate", args, &["--stamp"], &[])?;
            a.exactly(&["FILE"])?;
            Command::Validate { file: a.positional[0].clone(), stamp: a.on("--stamp") }
        }
        "draft" => {
            let a = Args::collect("draft", args, &[], &["--to"])?;
            a.at_least(&["FILES"])?;
            Command::Draft { files: a.positional.clone(), to: a.value("--to") }
        }
        "fit" => {
            let a = Args::collect("fit", args, &["--dry-run", "--propose"], &[])?;
            a.at_least(&["TARGET"])?;
            a.at_most(2)?;
            Command::Fit {
                target: a.positional[0].clone(),
                file: a.positional.get(1).cloned(),
                dry_run: a.on("--dry-run"),
                propose: a.on("--propose"),
            }
        }
        "check" => {
            let a = Args::collect("check", args, &[], &["--against"])?;
            a.exactly(&["TARGET"])?;
            Command::Check { target: a.positional[0].clone(), against: a.values_of("--against") }
        }
        "accept" => {
            let a = Args::collect("accept", args, &[], &[])?;
            a.exactly(&["TARGET", "MEMBER"])?;
            Command::Accept { target: a.positional[0].clone(), member: a.positional[1].clone() }
        }
        "output" => {
            let a = Args::collect("output", args, &["--force"], &["--format"])?;
            a.at_most(1)?;
            Command::Output {
                file: a.positional.first().cloned(),
                format: a.value("--format"),
                force: a.on("--force"),
            }
        }
        "schema" => {
            Args::collect("schema", args, &[], &[])?.exactly(&[])?;
            Command::Schema
        }
        "config" => {
            let a = Args::collect("config", args, &[], &[])?;
            a.exactly(&["init"])?;
            if a.positional[0] != "init" {
                return Err(ParseError::Unexpected { command: "config", token: a.positional[0].clone() });
            }
            Command::ConfigInit
        }
        "ls" => {
            let a = Args::collect("ls", args, &[], &[])?;
            a.at_most(1)?;
            Command::Ls { dir: a.positional.first().cloned() }
        }
        "cd" => {
            let a = Args::collect("cd", args, &[], &[])?;
            a.exactly(&["DIR"])?;
            Command::Cd { dir: a.positional[0].clone() }
        }
        "show" => {
            let a = Args::collect("show", args, &[], &[])?;
            a.exactly(&["FILE"])?;
            Command::Show { file: a.positional[0].clone() }
        }
        "edit" => {
            let a = Args::collect("edit", args, &[], &[])?;
            a.exactly(&["FILE"])?;
            Command::Edit { file: a.positional[0].clone() }
        }
        "help" => {
            let a = Args::collect("help", args, &[], &[])?;
            a.at_most(1)?;
            Command::Help { command: a.positional.first().cloned() }
        }
        "quit" | "exit" => {
            Args::collect(name_static(name), args, &[], &[])?.exactly(&[])?;
            Command::Quit
        }
        other => return Err(ParseError::Unknown(other.to_string())),
    })
}

fn name_static(n: &str) -> &'static str {
    if n == "exit" { "exit" } else { "quit" }
}
```

Note `Args::collect` treats any token beginning with `--` as a flag, so a file literally named `--x` is not addressable; that is acceptable and matches clap.

- [ ] **Step 4: Wire the module.** `src/console/mod.rs`:

```rust
//! The console: one grammar for the plain REPL, the batch runner and the
//! workbench. See docs/design/2026-09-01-console-and-workbench.md.

pub mod parse;

pub use parse::{parse, Command, ParseError};
```

`src/lib.rs`: add `pub mod console;` between `config` and `conform`, and a module-map line `//! - [\`console\`]  the dot-command grammar and session — \`tdy\` with no arguments`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib console::parse`
Expected: 8 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/console src/lib.rs
git commit -m "console::parse: the dot-command grammar, pure and table-tested"
```

---

### Task 3: Move `evidence` into the library

`.accept`'s first step returns evidence, so the computation must live where the session lives. It is a move, not a rewrite: the module already depends only on `tdy::*` and arrow.

**Files:**
- Move: `tdy-tui/src/evidence.rs` → `src/evidence.rs`
- Modify: `src/lib.rs` (`pub mod evidence;`), `tdy-tui/src/lib.rs` (`pub use tdy::evidence;` in place of `pub mod evidence;`)

**Interfaces:**
- Produces (unchanged API, new path): `tdy::evidence::{Evidence, Pair, for_spec}` with `pub fn for_spec(spec: &ParseSpec, path: &Path, limits: Limits, review: &str, model_framed: bool) -> Result<Vec<Evidence>>`.

- [ ] **Step 1: Move the file**

```bash
git mv tdy-tui/src/evidence.rs src/evidence.rs
```

- [ ] **Step 2: Fix the imports inside `src/evidence.rs`**: every `use tdy::` becomes `use crate::` (`tdy::config::Limits` → `crate::config::Limits`, `tdy::spec::…` → `crate::spec::…`, and any `tdy::engine`, `tdy::stream` references in the body — grep the file for `tdy::`). `datafusion::arrow` imports stay as they are (`datafusion` is a dependency of `tdy`).

- [ ] **Step 3: Register it.** `src/lib.rs`: add `pub mod evidence;` after `engine` and a module-map line `//! - [\`evidence\`] what accepting a reviewed member would do, computed over the whole file`. `tdy-tui/src/lib.rs`: replace `pub mod evidence;` with `pub use tdy::evidence;` — the TUI's `use tdy_tui::evidence` and `evidence::for_spec` call sites keep compiling.

- [ ] **Step 4: Build and run the whole workspace**

Run: `cargo test --workspace --lib --tests`
Expected: everything green; the evidence-related tests in `tdy-tui/tests/` and `tdy-tui/src/app.rs` unchanged and passing. If `evidence.rs` had `#[cfg(test)]` tests, they now run under `tdy` — confirm with `cargo test --lib evidence`.

- [ ] **Step 5: Commit**

```bash
git add -A src/evidence.rs tdy-tui/src src/lib.rs
git commit -m "evidence moves into the library: the console's .accept needs it before any UI does"
```

---

### Task 4: `commands.rs` — the CLI's text, returned instead of printed

The same-text promise (spec §4, §10) is only structural if the console and the CLI call one function. Today `sniff`/`validate` print inside `provider.rs`, and `fit TARGET FILE` / `check` print inside `main.rs`. This task moves the text production into `src/commands.rs`; the printing call sites become `print!("{text}")`.

**Files:**
- Create: `src/commands.rs`
- Modify: `src/lib.rs` (`pub mod commands;`), `src/provider.rs:694-764` (`sniff_command`, `validate_command`), `src/main.rs` (`fit_command` text path, `check_command` text path, `describe`, `print_proposals`)
- Test: existing suites are the regression net (`tests/fit.rs`, `tests/conform.rs`, `tests/adversarial.rs` read the binary's output); plus two focused tests in `tests/console.rs` created here.

**Interfaces:**
- Produces:

```rust
pub struct SniffOutcome {
    pub text: String,               // exactly what `tdy sniff` prints on stdout
    pub prepared: provider::PreparedFile,
    pub spec: spec::ParseSpec,
    pub kept_existing: bool,        // a fresh sidecar was reused (no --force)
}
pub async fn sniff_text(path: &Path, cfg: &Config, opts: provider::SniffCli<'_>) -> Result<SniffOutcome>;

pub fn validate_text(path: &Path, cfg: &Config, restamp: bool) -> Result<String>;

pub struct CheckOutcome { pub text: String, pub ok: bool }
pub fn check_text(target_path: &Path, files: &[PathBuf], limits: Limits) -> Result<CheckOutcome>;

pub struct FitOneOutcome { pub text: String, pub ok: bool, pub wrote: Option<PathBuf> }
pub async fn fit_one_text(target_path: &Path, file: &Path, cfg: &Config, dry_run: bool, propose: bool, progress: Option<&progress::Sink>) -> Result<FitOneOutcome>;

pub fn describe_dtype(d: &spec::DType) -> String;   // "DECIMAL(14,2)", "DATE  (%d.%m.%Y)" — moved from main.rs verbatim
```

Semantics to preserve exactly: the `ok = false` cases are the ones where the CLI today `bail!`s **after** printing (check with non-conforming files; fit-one with gaps). The bail message itself stays in `main.rs` (it is printed by `main` as `Error: …`), so `text` contains everything printed *before* the error and `ok` tells the caller to raise it. The console composes `text + "Error: " + message` (Task 7).

- [ ] **Step 1: Write the failing tests** — create `tests/console.rs`:

```rust
//! The console: grammar, session, and the promise that its text is the CLI's.

use std::path::{Path, PathBuf};
use std::process::Command;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

/// A scratch copy of the drifting-exports pile: data files only, no
/// sidecars, no locks, no targets (each test writes the target it needs).
fn pile() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(corpus()).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("2025-") && !n.ends_with(".tdy.toml") {
            std::fs::copy(e.path(), d.path().join(&n)).unwrap();
        }
    }
    std::fs::copy(corpus().join("sales.tdy.sql"), d.path().join("sales.tdy.sql")).unwrap();
    d
}

fn tdy(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(args)
        .current_dir(dir)
        .env("TDY_BACKEND", "none")
        .output()
        .expect("run tdy")
}

fn no_llm() -> tdy::config::Config {
    tdy::config::load(&tdy::config::Overrides { backend: Some("none".into()), model: None, base_url: None })
        .unwrap()
}

#[tokio::test]
async fn sniff_text_is_what_the_binary_prints() {
    let d = pile();
    let cli = tdy(d.path(), &["sniff", "2025-01.csv", "--no-llm"]);
    assert!(cli.status.success());
    std::fs::remove_file(d.path().join("2025-01.csv.tdy.toml")).unwrap();

    let out = tdy::commands::sniff_text(
        &d.path().join("2025-01.csv"),
        &no_llm(),
        tdy::provider::SniffCli { hint: None, force: false, no_llm: true, quick: false, json: false },
    )
    .await
    .unwrap();
    // The sidecar text embeds created_at, so compare with timestamps masked.
    let mask = |s: &str| {
        s.lines()
            .filter(|l| !l.starts_with("created_at"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(mask(&out.text), mask(&String::from_utf8_lossy(&cli.stdout)));
    assert!(!out.kept_existing);
    assert_eq!(out.spec.columns.len(), 3);
}

#[test]
fn check_text_matches_binary_including_failure() {
    let d = pile();
    // No lock yet: the "nothing to check" wording.
    let cli = tdy(d.path(), &["check", "sales.tdy.sql"]);
    let out = tdy::commands::check_text(&d.path().join("sales.tdy.sql"), &[], no_llm().limits).unwrap();
    assert_eq!(out.text, String::from_utf8_lossy(&cli.stdout));
    assert!(out.ok);
}
```

`Overrides`' fields: check `src/config.rs:257` — if its shape differs from `{ backend, model, base_url }`, use the real one. `TDY_BACKEND=none` is the environment form of the same override; confirm the variable name in `config::EnvVars::from_process` (`src/config.rs:285-300`) and use whatever it is.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test console`
Expected: compile error — `tdy::commands` does not exist.

- [ ] **Step 3: Create `src/commands.rs`** by *moving* code, not rewriting it:

```rust
//! The CLI's subcommands as functions that return their text.
//!
//! `tdy sniff` and the console's `.sniff` must print the same thing, and the
//! only way that stays true is if there is one function producing it. So
//! the text lives here, `main.rs` prints it, and `console` returns it. Nothing
//! in this module writes to stdout or stderr.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::config::{Backend, Config, Limits};
use crate::provider::{self, PreparedFile, SniffCli};
use crate::spec::{DType, InferenceMethod, ParseSpec};
use crate::{engine, sidecar, sniff};

pub struct SniffOutcome {
    pub text: String,
    pub prepared: PreparedFile,
    pub spec: ParseSpec,
    pub kept_existing: bool,
}

/// `tdy sniff`'s text. Body lifted from `provider::sniff_command`, with
/// `println!` replaced by writes into `text`.
pub async fn sniff_text(path: &Path, cfg: &Config, opts: SniffCli<'_>) -> Result<SniffOutcome> {
    let SniffCli { hint, force, no_llm, quick, .. } = opts;
    let cfg = if no_llm {
        let mut c = cfg.clone();
        c.backend = Backend::None;
        c
    } else {
        cfg.clone()
    };
    // Said out loud rather than discovered: a fresh sidecar is kept unless
    // --force, and a reader who just ran .fit and comes back to .sniff should
    // learn that from the output, not from column names that changed.
    let kept_existing =
        !force && matches!(sidecar::load(path), Ok(sidecar::SidecarStatus::Fresh(_)));
    let prepared =
        provider::ensure_sidecar_opts(path, &cfg, hint, force, sniff::SniffOpts { verify: !quick })
            .await?;
    let sc_path = sidecar::sidecar_path(path);
    let mut text = String::new();
    if kept_existing {
        writeln!(text, "note: {} is fresh and was kept; --force to re-infer", sc_path.display())?;
    }
    writeln!(text, "# {}", sc_path.display())?;
    writeln!(text, "{}", std::fs::read_to_string(&sc_path)?)?;
    let spec = sidecar::load(path)?
        .fresh_spec()
        .ok_or_else(|| anyhow!("internal: sidecar not fresh right after writing it"))?;
    let batch = engine::preview(&spec, path, cfg.limits, 10)?;
    writeln!(
        text,
        "preview ({} method, confidence {}):",
        match prepared.method {
            InferenceMethod::Heuristic => "heuristic",
            InferenceMethod::Llm => "llm",
            InferenceMethod::Manual => "manual",
        },
        prepared.confidence.map(|c| format!("{c:.2}")).unwrap_or_else(|| "n/a".into())
    )?;
    writeln!(text, "{}", datafusion::arrow::util::pretty::pretty_format_batches(&[batch])?)?;
    Ok(SniffOutcome { text, prepared, spec, kept_existing })
}

/// `tdy validate`'s text. Body lifted from `provider::validate_command`.
pub fn validate_text(path: &Path, cfg: &Config, restamp: bool) -> Result<String> {
    let sc_path = sidecar::sidecar_path(path);
    let notes = provider::validate_quiet(path, cfg, restamp)?;
    let mut text = String::new();
    if restamp {
        writeln!(text, "re-fingerprinted {} (method = manual)", sc_path.display())?;
    }
    writeln!(text, "{}: ok", sc_path.display())?;
    for n in &notes {
        writeln!(text, "  note: {n}")?;
    }
    Ok(text)
}

pub struct CheckOutcome {
    pub text: String,
    pub ok: bool,
}

/// `tdy check`'s text path. Body lifted from `main.rs::check_command` (the
/// non-JSON branch): every `println!` becomes a `writeln!(text, …)`, and the
/// two `bail!` sites become `ok = false` with the same wording left to the
/// caller (see `main.rs`, which bails with the identical sentence).
pub fn check_text(target_path: &Path, files: &[PathBuf], limits: Limits) -> Result<CheckOutcome> {
    /* move the body of main.rs::check_command here, JSON branch excluded */
    todo!("moved in step 3 — see instructions below")
}

pub struct FitOneOutcome {
    pub text: String,
    pub ok: bool,
    pub wrote: Option<PathBuf>,
}

/// `tdy fit TARGET FILE`'s text path. Body lifted from `main.rs::fit_command`
/// (the non-JSON branch). `print_proposals` moves here too, as
/// `write_proposals(&mut text, …)`.
pub async fn fit_one_text(
    target_path: &Path,
    file: &Path,
    cfg: &Config,
    dry_run: bool,
    propose: bool,
    progress: Option<&crate::progress::Sink>,
) -> Result<FitOneOutcome> {
    todo!("moved in step 3 — see instructions below")
}

/// The SQL-ish spelling of a dtype, as the CLI prints it. Moved from main.rs.
pub fn describe_dtype(d: &DType) -> String {
    match d {
        DType::Utf8 => "TEXT".into(),
        DType::Bool => "BOOLEAN".into(),
        DType::Int64 => "BIGINT".into(),
        DType::Float64 => "DOUBLE".into(),
        DType::Decimal { precision, scale } => format!("DECIMAL({precision},{scale})"),
        DType::Date { format } => format!("DATE  ({format})"),
        DType::Timestamp { format, timezone } => match timezone {
            Some(tz) => format!("TIMESTAMP  ({format}, {tz})"),
            None => format!("TIMESTAMP  ({format})"),
        },
    }
}
```

The two `todo!()`s are to be replaced **in this same step** by the moved bodies — they are shown as `todo!` here only because the bodies are ~150 lines that already exist verbatim in `main.rs`. Concretely:

- `check_text`: copy `main.rs::check_command`'s body from `let target = Target::load(target_path)?;` onward, drop the `if json { return check_json(...) }` lines, replace each `println!(` with `writeln!(text, ` (and `print!` with `write!`), and replace the final `if bad > 0 { anyhow::bail!(…) }` with `Ok(CheckOutcome { text, ok: bad == 0 })`; the early `return Ok(())` paths become `return Ok(CheckOutcome { text, ok: true })`.
- `fit_one_text`: copy `main.rs::fit_command`'s body, keep only the non-JSON branches, replace prints with writes, and: on the `Ok(planned)` path return `ok: true, wrote: Some(path)` (or `wrote: None` when `dry_run`); on `Err(FitError::Gaps(gaps))` write the gap text (and proposals when `propose`), return `ok: false, wrote: None`; on `Err(e)` write `format!("{e}")` and return `ok: false`. `main.rs::print_proposals` becomes `fn write_proposals(text: &mut String, file, target, limits)` in this module. Pass `progress` through to `fit::plan` (today `Some(&stderr_sink())` is hard-coded; the caller now decides).
- `describe` in `main.rs` is deleted; its remaining call sites (the JSON branch of `fit_command`, if any) use `tdy::commands::describe_dtype`.

- [ ] **Step 4: Rewire the printing call sites.**

`src/provider.rs`:
```rust
pub async fn sniff_command(path: &Path, cfg: &Config, opts: SniffCli<'_>) -> Result<()> {
    if opts.json {
        // unchanged JSON branch: ensure_sidecar_opts + sniff_json_value + println!
        …
        return Ok(());
    }
    print!("{}", crate::commands::sniff_text(path, cfg, opts).await?.text);
    Ok(())
}
pub fn validate_command(path: &Path, cfg: &Config, restamp: bool) -> Result<()> {
    print!("{}", crate::commands::validate_text(path, cfg, restamp)?);
    Ok(())
}
```

`src/main.rs`:
```rust
        Command::Check { target, against } => {
            let cfg = config::load(&overrides)?;
            if cli.json {
                check_json(&target, &tdy::target::Target::load(&target)?, &against, cfg.limits)?;
            } else {
                let out = tdy::commands::check_text(&target, &against, cfg.limits)?;
                print!("{}", out.text);
                if !out.ok {
                    anyhow::bail!("{} file(s) do not produce the declared schema", /* count */);
                }
            }
        }
```
The bail count: `check_text`'s text already ends with "`N of M file(s) conform`", but the bail needs the bad count — add `pub bad: usize` to `CheckOutcome` and use it. Same pattern for `fit_command`: keep the JSON branch in `main.rs`, and for text call `fit_one_text(…, Some(&tdy::progress::stderr_sink()))`, print `text`, and bail with the existing sentence (`"no plan reaches the declared schema"` for gaps, `"could not fit {file}"` otherwise) when `!ok` — to tell the two apart add `pub gaps: bool` to `FitOneOutcome`.

- [ ] **Step 5: Run everything**

Run: `cargo test --workspace --lib --tests`
Expected: green, including the two new tests in `tests/console.rs`. Then eyeball one command for byte-identical output against the previous binary: `git stash; cargo run -q -- check testdata/drifting_exports/sales.tdy.sql > /tmp/before.txt; git stash pop; cargo run -q -- check testdata/drifting_exports/sales.tdy.sql > /tmp/after.txt; diff /tmp/before.txt /tmp/after.txt` (use the scratchpad directory rather than `/tmp` in this environment). Expected: no diff.

- [ ] **Step 6: Commit**

```bash
git add src/commands.rs src/provider.rs src/main.rs src/lib.rs tests/console.rs
git commit -m "commands.rs: the CLI's text is returned, not printed, so the console can speak it verbatim"
```

---

### Task 5: `console::Session` — the skeleton: `.help`, `.quit`, `.cd`, `.ls`, errors, confinement

**Files:**
- Modify: `src/console/mod.rs`
- Test: `tests/console.rs`

**Interfaces:**
- Produces:

```rust
pub struct Session { /* private */ }

impl Session {
    /// `root` is canonicalised; `cwd` starts at root. `cfg` is what the
    /// commands run with (backend, limits).
    pub fn new(root: &Path, cfg: Config) -> Result<Session>;
    pub fn root(&self) -> &Path;
    pub fn cwd(&self) -> &Path;
    pub fn cfg(&self) -> &Config;
    /// Set by `.quit`; frontends check it after each `run`.
    pub fn wants_quit(&self) -> bool;
    /// One line in, one outcome out. Never panics on input; never prints.
    pub async fn run(&mut self, line: &str, progress: Option<&progress::Sink>) -> Outcome;
    /// Resolve a user-supplied path against cwd and confine it to root.
    pub fn resolve(&self, p: &str) -> Result<PathBuf>;
    /// Globs expanded against cwd, each result confined. Errors if a glob
    /// matched nothing.
    pub fn expand(&self, patterns: &[String]) -> Result<Vec<PathBuf>>;
}

#[derive(Debug)]
pub struct Outcome {
    pub echo: String,
    pub text: String,
    pub payload: Payload,
    pub ok: bool,
}

#[derive(Debug)]
pub enum Payload {
    Nothing,
    /// An incomplete SQL statement was buffered; nothing ran.
    Continue,
    Quit,
    Listing(Vec<Entry>),
    Shown { path: PathBuf, raw: RawHead, spec: Option<SpecSummary> },
    Sniffed { path: PathBuf, spec: SpecSummary, preview: Table, kept_existing: bool },
    Drafted { ddl: String, wrote: Option<PathBuf> },
    Fitted(crate::report::PileReport),
    Evidence { target: PathBuf, member: String, rows: Vec<crate::evidence::Evidence> },
    Query(Table),
    /// The frontend runs `$EDITOR` on this path (the session cannot own the terminal).
    Edit(PathBuf),
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,            // relative to the listed directory; dirs end with '/'
    pub kind: EntryKind,
    pub status: EntryStatus,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind { Dir, File, Target }
#[derive(Debug, Clone, PartialEq)]
pub enum EntryStatus {
    None,                                  // a dir, or a file with no sidecar
    Sniffed { confidence: Option<f32>, method: String },
    Stale,
    NoLock,                                // target without a lock
    Locked,                                // target with a lock and no drift
    Drift(usize),                          // target whose lock disagrees in N places
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub types: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub total: usize,
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub struct RawHead { pub lines: Vec<String>, pub truncated: bool, pub sheets: Vec<(String, usize, usize)> }
#[derive(Debug, Clone, PartialEq)]
pub struct SpecSummary {
    pub method: String,
    pub confidence: Option<f32>,
    pub extraction: String,          // compact JSON of spec.extraction
    pub transforms: Vec<String>,     // compact JSON per transform, in order
    pub columns: Vec<(String, String, String)>,   // (name, source, dtype as describe_dtype)
    pub notes: Vec<String>,
}

/// Which files the browser and `.ls` treat as data (by extension).
pub fn is_data_file(name: &str) -> bool;   // csv tsv txt log json ndjson jsonl xlsx xlsm xls xlsb ods
pub fn is_target(name: &str) -> bool;      // ends with .tdy.sql
pub fn render_listing(entries: &[Entry]) -> String;   // the `.ls` text: "name  status" per line, aligned
```

`Entry`, `Table`, `RawHead`, `SpecSummary` are what slice 2 draws; defining them now is what makes slice 2 a UI task.

- [ ] **Step 1: Write the failing tests** (append to `tests/console.rs`):

```rust
use tdy::console::{EntryKind, EntryStatus, Payload, Session};

async fn session(dir: &Path) -> Session {
    Session::new(dir, no_llm()).unwrap()
}

#[tokio::test]
async fn help_quit_and_unknown() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".help", None).await;
    assert!(o.ok);
    assert!(o.text.contains(".sniff FILE") && o.text.contains(".fit TARGET"));
    let o = s.run(".nope", None).await;
    assert!(!o.ok);
    assert_eq!(o.text, "Error: unknown command `.nope` — `.help` lists them\n");
    assert!(matches!(o.payload, Payload::Error { .. }));
    let o = s.run(".quit", None).await;
    assert!(matches!(o.payload, Payload::Quit) && s.wants_quit());
}

#[tokio::test]
async fn ls_hides_companions_and_reports_status() {
    let d = pile();
    std::fs::create_dir(d.path().join("archive")).unwrap();
    let mut s = session(d.path()).await;
    s.run(".sniff 2025-01.csv --no-llm", None).await;
    // Stale: sidecar written, then the file changes.
    s.run(".sniff 2025-02.csv --no-llm", None).await;
    std::fs::write(d.path().join("2025-02.csv"), "Datum;Region;Betrag\n01.02.2025;Ost;1\n").unwrap();

    let o = s.run(".ls", None).await;
    assert!(o.ok);
    let Payload::Listing(entries) = o.payload else { panic!("{:?}", o.payload) };
    let find = |n: &str| entries.iter().find(|e| e.name == n).unwrap_or_else(|| panic!("{n} missing"));
    assert_eq!(find("archive/").kind, EntryKind::Dir);
    assert!(matches!(find("2025-01.csv").status, EntryStatus::Sniffed { .. }));
    assert_eq!(find("2025-02.csv").status, EntryStatus::Stale);
    assert_eq!(find("2025-07.csv").status, EntryStatus::None);
    assert_eq!(find("sales.tdy.sql").kind, EntryKind::Target);
    assert_eq!(find("sales.tdy.sql").status, EntryStatus::NoLock);
    assert!(entries.iter().all(|e| !e.name.ends_with(".tdy.toml")));
    assert!(o.text.contains("2025-02.csv") && o.text.contains("stale"));
}

#[tokio::test]
async fn cd_stays_inside_the_root() {
    let d = pile();
    std::fs::create_dir(d.path().join("archive")).unwrap();
    let mut s = session(d.path()).await;
    assert!(s.run(".cd archive", None).await.ok);
    assert!(s.cwd().ends_with("archive"));
    assert!(s.run(".cd ..", None).await.ok);
    let o = s.run(".cd ..", None).await;
    assert!(!o.ok && o.text.contains("outside"));
    let o = s.run(".sniff ../../etc/passwd", None).await;
    assert!(!o.ok && o.text.contains("outside"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test console`
Expected: compile errors — `Session` etc. missing.

- [ ] **Step 3: Implement the skeleton in `src/console/mod.rs`**

```rust
pub mod parse;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::progress;
pub use parse::{parse, Command, ParseError};

/* Outcome, Payload, Entry, EntryKind, EntryStatus, Table, RawHead, SpecSummary — as in Interfaces */

pub struct Session {
    root: PathBuf,
    cwd: PathBuf,
    cfg: Config,
    quit: bool,
    sql_buffer: String,
    output: Option<(PathBuf, crate::provider::OutputFormat)>,   // Task 8
    pending_accept: Option<(PathBuf, String)>,                   // Task 9
}

impl Session {
    pub fn new(root: &Path, cfg: Config) -> Result<Session> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot open root {}", root.display()))?;
        Ok(Session { cwd: root.clone(), root, cfg, quit: false, sql_buffer: String::new(), output: None, pending_accept: None })
    }
    pub fn root(&self) -> &Path { &self.root }
    pub fn cwd(&self) -> &Path { &self.cwd }
    pub fn cfg(&self) -> &Config { &self.cfg }
    pub fn wants_quit(&self) -> bool { self.quit }

    pub fn resolve(&self, p: &str) -> Result<PathBuf> {
        let joined = self.cwd.join(p);
        crate::fileio::confine(&joined, &self.root)
            .map_err(|_| anyhow::anyhow!("{p}: outside the console's root {}", self.root.display()))
    }

    pub fn expand(&self, patterns: &[String]) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for pat in patterns {
            let hits = crate::lockfile::expand_glob(&self.cwd, pat)?;
            if hits.is_empty() {
                bail!("{pat}: no file matches");
            }
            for h in hits {
                out.push(crate::fileio::confine(&h, &self.root).map_err(|_| {
                    anyhow::anyhow!("{}: outside the console's root {}", h.display(), self.root.display())
                })?);
            }
        }
        Ok(out)
    }

    pub async fn run(&mut self, line: &str, progress: Option<&progress::Sink>) -> Outcome {
        let trimmed = line.trim();
        // SQL assembly (Task 8) goes here; for now every non-dot line is
        // an error so the tests for this task do not depend on it.
        let cmd = match parse(trimmed) {
            Ok(c) => c,
            Err(e) => return Outcome::error(trimmed, e.to_string()),
        };
        let echo = trimmed.to_string();
        match self.dispatch(cmd, progress).await {
            Ok(mut o) => {
                if o.echo.is_empty() {
                    o.echo = echo;
                }
                o
            }
            Err(e) => Outcome::error(&echo, format!("{e:#}")),
        }
    }

    async fn dispatch(&mut self, cmd: Command, progress: Option<&progress::Sink>) -> Result<Outcome> {
        Ok(match cmd {
            Command::Help { command } => Outcome::ok(help_text(command.as_deref()), Payload::Nothing),
            Command::Quit => {
                self.quit = true;
                Outcome::ok(String::new(), Payload::Quit)
            }
            Command::Cd { dir } => {
                let p = self.resolve(&dir)?;
                if !p.is_dir() {
                    bail!("{dir}: not a directory");
                }
                self.cwd = p.canonicalize()?;
                // Relative paths inside SQL (`messy('2025-01.csv')`) resolve
                // against the process's directory; a console is one session
                // per process, so moving the process is the honest thing.
                std::env::set_current_dir(&self.cwd)?;
                Outcome::ok(format!("{}\n", self.display_rel(&self.cwd)), Payload::Nothing)
            }
            Command::Ls { dir } => {
                let p = match dir {
                    Some(d) => self.resolve(&d)?,
                    None => self.cwd.clone(),
                };
                let entries = list_dir(&p)?;
                Outcome::ok(render_listing(&entries), Payload::Listing(entries))
            }
            other => bail!("`{}` is not implemented yet", describe_command(&other)),
        })
    }

    /// Root-relative display of a path, "." for the root itself.
    fn display_rel(&self, p: &Path) -> String {
        match p.strip_prefix(&self.root) {
            Ok(r) if r.as_os_str().is_empty() => ".".into(),
            Ok(r) => r.display().to_string(),
            Err(_) => p.display().to_string(),
        }
    }
}

impl Outcome {
    fn ok(text: String, payload: Payload) -> Outcome {
        Outcome { echo: String::new(), text, payload, ok: true }
    }
    fn error(echo: &str, message: String) -> Outcome {
        Outcome {
            echo: echo.to_string(),
            text: format!("Error: {message}\n"),
            payload: Payload::Error { message },
            ok: false,
        }
    }
}

fn describe_command(c: &Command) -> String { format!("{c:?}") }

pub fn is_data_file(name: &str) -> bool {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "csv" | "tsv" | "txt" | "log" | "json" | "ndjson" | "jsonl" | "xlsx" | "xlsm" | "xls" | "xlsb" | "ods")
}
pub fn is_target(name: &str) -> bool { name.ends_with(".tdy.sql") }

/// Directory listing the way the browser shows it: dirs first, then files,
/// each sorted; companions folded into their owner's status.
pub fn list_dir(dir: &Path) -> Result<Vec<Entry>> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') { continue; }
        let path = e.path();
        if path.is_dir() {
            dirs.push(Entry { name: format!("{name}/"), kind: EntryKind::Dir, status: EntryStatus::None });
        } else if is_target(&name) {
            files.push(Entry { name, kind: EntryKind::Target, status: target_status(&path) });
        } else if is_data_file(&name) && !name.ends_with(".tdy.toml") {
            files.push(Entry { name, kind: EntryKind::File, status: file_status(&path) });
        }
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.extend(files);
    Ok(dirs)
}

fn file_status(path: &Path) -> EntryStatus {
    use crate::sidecar::SidecarStatus;
    match crate::sidecar::load(path) {
        Ok(SidecarStatus::Fresh(sc)) => EntryStatus::Sniffed {
            confidence: sc.spec.confidence,
            method: format!("{:?}", sc.provenance.method).to_lowercase(),
        },
        Ok(SidecarStatus::Stale(_)) => EntryStatus::Stale,
        Ok(SidecarStatus::Absent) => EntryStatus::None,
        Err(_) => EntryStatus::Stale,   // unreadable sidecar: not something a query would use
    }
}

fn target_status(path: &Path) -> EntryStatus {
    let Ok(target) = crate::target::Target::load(path) else { return EntryStatus::NoLock };
    match crate::lockfile::Lock::load(path) {
        Ok(Some(lock)) => match crate::lockfile::drift(&lock, &target, path) {
            Ok(d) if d.is_empty() => EntryStatus::Locked,
            Ok(d) => EntryStatus::Drift(d.len()),
            Err(_) => EntryStatus::Drift(1),
        },
        _ => EntryStatus::NoLock,
    }
}

pub fn render_listing(entries: &[Entry]) -> String {
    let width = entries.iter().map(|e| e.name.chars().count()).max().unwrap_or(0);
    let mut s = String::new();
    for e in entries {
        let status = match &e.status {
            EntryStatus::None => String::new(),
            EntryStatus::Sniffed { confidence: Some(c), method } => format!("sniffed {c:.2} ({method})"),
            EntryStatus::Sniffed { confidence: None, method } => format!("sniffed ({method})"),
            EntryStatus::Stale => "stale".into(),
            EntryStatus::NoLock => "target, no lock".into(),
            EntryStatus::Locked => "target, locked".into(),
            EntryStatus::Drift(n) => format!("target, drift ({n})"),
        };
        let _ = writeln!(s, "{:<width$}  {status}", e.name, width = width);
    }
    s
}

fn help_text(command: Option<&str>) -> String {
    const ALL: &str = "\
SQL runs as typed; end a statement with `;` (it may span lines).
Everything else is a dot-command:

  .sniff FILE [--quick] [--force] [--no-llm] [--hint \"…\"]   infer the sidecar for one file
  .validate FILE [--stamp]                                  check a sidecar against its file
  .draft FILES… [--to NAME.tdy.sql]                         draft a target from a pile
  .fit TARGET [FILE] [--dry-run] [--propose]                plan every member onto a target
  .check TARGET [--against FILE…]                           the CI gate
  .accept TARGET MEMBER                                     show the evidence; again to accept
  .output [FILE] [--format parquet|csv] [--force]           route the next result to a file
  .show FILE          the raw head beside what the sidecar says
  .ls [DIR]  .cd DIR  .edit FILE  .schema  .config init  .help [CMD]  .quit
";
    match command {
        None => ALL.to_string(),
        Some(c) => ALL
            .lines()
            .filter(|l| l.trim_start().starts_with(&format!(".{c}")))
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .trim_end()
            .to_string()
            + "\n",
    }
}
```

`sc.spec.confidence` — confirm the field name and type in `spec.rs` (`ParseSpec.confidence: Option<f32>`? The sidecar TOML shows `confidence = 0.95` under `[spec]`). `Provenance.method` derives `Debug`; if it also derives `Serialize` with `rename_all = "lowercase"`, prefer `serde_json::to_string(&m)` trimmed of quotes for the method label. Check `Target::load` exists (used in `report.rs:250`) — it does.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test console`
Expected: `help_quit_and_unknown`, `cd_stays_inside_the_root` pass; `ls_hides_companions_and_reports_status` **fails** on the `.sniff` lines with "not implemented yet" — that is Task 6. Temporarily mark it `#[ignore]` with a comment `// enabled in Task 6`, and commit.

- [ ] **Step 5: Commit**

```bash
git add src/console/mod.rs tests/console.rs
git commit -m "console::Session: the skeleton — help, quit, cd, ls, errors as outcomes, every path confined"
```

---

### Task 6: `.sniff`, `.validate`, `.show`

**Files:**
- Modify: `src/console/mod.rs`
- Test: `tests/console.rs` (un-ignore `ls_hides_companions_and_reports_status`; add the tests below)

**Interfaces:**
- Consumes: `commands::{sniff_text, validate_text, describe_dtype}` (Task 4); `engine::preview`, `engine::excel_sheet_shapes` (`SheetShape { name, rows, cols, .. }`).
- Produces: `pub fn spec_summary(spec: &ParseSpec, method: &str, confidence: Option<f32>) -> SpecSummary`; `pub fn table_of(schema: &Schema, batches: &[RecordBatch], cap: usize) -> Table`; `pub fn raw_head(path: &Path, limits: Limits) -> Result<RawHead>`.

- [ ] **Step 1: Write the failing tests** (append to `tests/console.rs`, and remove the `#[ignore]` from Task 5's ls test):

```rust
#[tokio::test]
async fn sniff_writes_the_sidecar_and_returns_a_summary() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".sniff 2025-01.csv --no-llm", None).await;
    assert!(o.ok, "{}", o.text);
    assert_eq!(o.echo, ".sniff 2025-01.csv --no-llm");
    assert!(d.path().join("2025-01.csv.tdy.toml").exists());
    let Payload::Sniffed { spec, preview, kept_existing, .. } = o.payload else { panic!() };
    assert!(!kept_existing);
    assert_eq!(spec.columns.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["datum", "region", "betrag"]);
    assert_eq!(spec.columns[2].2, "DECIMAL(38,2)");
    assert_eq!(preview.columns, ["datum", "region", "betrag"]);
    assert_eq!(preview.rows[0], ["2025-01-31", "Ost", "1100.00"]);
    assert!(o.text.contains("preview (heuristic method, confidence 0.95)"));

    // A second sniff keeps the fresh sidecar and says so.
    let o = s.run(".sniff 2025-01.csv --no-llm", None).await;
    assert!(o.ok);
    assert!(o.text.starts_with("note: ") && o.text.contains("--force to re-infer"));
    let Payload::Sniffed { kept_existing, .. } = o.payload else { panic!() };
    assert!(kept_existing);
}

#[tokio::test]
async fn validate_and_show() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".validate 2025-01.csv", None).await;
    assert!(!o.ok && o.text.contains("no sidecar"));
    s.run(".sniff 2025-01.csv --no-llm", None).await;
    let o = s.run(".validate 2025-01.csv", None).await;
    assert!(o.ok && o.text.contains(": ok"));

    let o = s.run(".show 2025-07.csv", None).await;   // no sidecar
    assert!(o.ok, "{}", o.text);
    let Payload::Shown { raw, spec, .. } = o.payload else { panic!() };
    assert_eq!(raw.lines[0], "Datum;Region;Betrag Rp.");
    assert!(spec.is_none());
    assert!(o.text.contains("Datum;Region;Betrag Rp.") && o.text.contains("no sidecar"));

    let o = s.run(".show 2025-01.csv", None).await;   // with sidecar
    let Payload::Shown { spec: Some(sp), .. } = o.payload else { panic!() };
    assert_eq!(sp.columns[0].1, "Datum");

    let o = s.run(".show 2025-09.xlsx", None).await;
    let Payload::Shown { raw, .. } = o.payload else { panic!() };
    assert_eq!(raw.sheets.len(), 1);
    assert_eq!(raw.sheets[0].0, "Umsatz");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test console sniff validate ls`
Expected: fail with "not implemented yet".

- [ ] **Step 3: Implement** — add to `dispatch`:

```rust
            Command::Sniff { file, quick, force, no_llm, hint } => {
                let path = self.resolve(&file)?;
                let out = crate::commands::sniff_text(
                    &path,
                    &self.cfg,
                    crate::provider::SniffCli { hint: hint.as_deref(), force, no_llm, quick, json: false },
                )
                .await?;
                let method = method_label(out.prepared.method);
                let summary = spec_summary(&out.spec, &method, out.prepared.confidence);
                let batch = crate::engine::preview(&out.spec, &path, self.cfg.limits, 10)?;
                let preview = table_of(&batch.schema(), std::slice::from_ref(&batch), 10);
                Outcome::ok(out.text, Payload::Sniffed { path, spec: summary, preview, kept_existing: out.kept_existing })
            }
            Command::Validate { file, stamp } => {
                let path = self.resolve(&file)?;
                Outcome::ok(crate::commands::validate_text(&path, &self.cfg, stamp)?, Payload::Nothing)
            }
            Command::Show { file } => {
                let path = self.resolve(&file)?;
                let raw = raw_head(&path, self.cfg.limits)?;
                let spec = match crate::sidecar::load(&path)? {
                    crate::sidecar::SidecarStatus::Fresh(sc) => {
                        Some(spec_summary(&sc.spec, &method_label(sc.provenance.method), sc.spec.confidence))
                    }
                    _ => None,
                };
                let text = render_shown(&file, &raw, spec.as_ref());
                Outcome::ok(text, Payload::Shown { path, raw, spec })
            }
```

and the helpers:

```rust
fn method_label(m: crate::spec::InferenceMethod) -> String {
    use crate::spec::InferenceMethod::*;
    match m { Heuristic => "heuristic", Llm => "llm", Manual => "manual" }.into()
}

pub fn spec_summary(spec: &crate::spec::ParseSpec, method: &str, confidence: Option<f32>) -> SpecSummary {
    SpecSummary {
        method: method.to_string(),
        confidence,
        extraction: serde_json::to_string(&spec.extraction).unwrap_or_default(),
        transforms: spec.transforms.iter().map(|t| serde_json::to_string(t).unwrap_or_default()).collect(),
        columns: spec
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.source_name().to_string(), crate::commands::describe_dtype(&c.dtype)))
            .collect(),
        notes: spec.notes.clone(),
    }
}

pub fn table_of(
    schema: &datafusion::arrow::datatypes::Schema,
    batches: &[datafusion::arrow::record_batch::RecordBatch],
    cap: usize,
) -> Table {
    use datafusion::arrow::util::display::array_value_to_string;
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    let mut rows = Vec::new();
    'outer: for b in batches {
        for i in 0..b.num_rows() {
            if rows.len() >= cap {
                break 'outer;
            }
            rows.push(
                (0..b.num_columns())
                    .map(|c| {
                        let col = b.column(c);
                        if col.is_null(i) { String::new() } else { array_value_to_string(col, i).unwrap_or_default() }
                    })
                    .collect(),
            );
        }
    }
    Table {
        columns: schema.fields().iter().map(|f| f.name().clone()).collect(),
        types: schema.fields().iter().map(|f| f.data_type().to_string()).collect(),
        truncated: total > rows.len(),
        rows,
        total,
    }
}

const HEAD_BYTES: usize = 16 * 1024;
const HEAD_LINES: usize = 40;
const WORKBOOK_EXT: [&str; 5] = ["xlsx", "xlsm", "xls", "xlsb", "ods"];

pub fn raw_head(path: &Path, limits: crate::config::Limits) -> Result<RawHead> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if WORKBOOK_EXT.contains(&ext.as_str()) {
        let sheets = crate::engine::excel_sheet_shapes(path, limits)?
            .into_iter()
            .map(|s| (s.name, s.rows, s.cols))
            .collect();
        return Ok(RawHead { lines: Vec::new(), truncated: false, sheets });
    }
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut buf = Vec::with_capacity(HEAD_BYTES);
    f.by_ref().take(HEAD_BYTES as u64).read_to_end(&mut buf)?;
    let more = f.metadata().map(|m| m.len() as usize > buf.len()).unwrap_or(false);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    if more && !text.ends_with('\n') {
        lines.pop();   // a torn last line is not a line
    }
    let truncated = more || lines.len() > HEAD_LINES;
    lines.truncate(HEAD_LINES);
    Ok(RawHead { lines, truncated, sheets: Vec::new() })
}

fn render_shown(name: &str, raw: &RawHead, spec: Option<&SpecSummary>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{name}:");
    if raw.sheets.is_empty() {
        for l in &raw.lines {
            let _ = writeln!(s, "  {l}");
        }
        if raw.truncated {
            let _ = writeln!(s, "  …");
        }
    } else {
        for (n, r, c) in &raw.sheets {
            let _ = writeln!(s, "  sheet {n:?}: {r} row(s) x {c} col(s)");
        }
    }
    match spec {
        None => { let _ = writeln!(s, "\nno sidecar — `.sniff {name}` to infer one"); }
        Some(sp) => {
            let _ = writeln!(s, "\nsidecar ({} method, confidence {}):", sp.method,
                sp.confidence.map(|c| format!("{c:.2}")).unwrap_or_else(|| "n/a".into()));
            let _ = writeln!(s, "  extraction  {}", sp.extraction);
            for t in &sp.transforms { let _ = writeln!(s, "  transform   {t}"); }
            for (n, src, ty) in &sp.columns { let _ = writeln!(s, "  {n:<16} <- {src:<24} {ty}"); }
            for n in &sp.notes { let _ = writeln!(s, "  note: {n}"); }
        }
    }
    s
}
```

`spec.extraction` and each `Transform` derive `Serialize` (they are the JSON-Schema source), so `serde_json::to_string` is available. If `ParseSpec.confidence` is not `Option<f32>`, adapt `SpecSummary.confidence` to match rather than converting.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test console`
Expected: all pass including the un-ignored ls test.

- [ ] **Step 5: Commit**

```bash
git add src/console/mod.rs tests/console.rs
git commit -m "console: .sniff, .validate, .show — the single-file commands, with the summary slice 2 will draw"
```

---

### Task 7: `.draft --to`, `.fit`, `.check`, `.schema`, `.config init`, `.edit`

**Files:**
- Modify: `src/console/mod.rs`
- Test: `tests/console.rs`

**Interfaces:**
- Consumes: `draft::draft_target(&[PathBuf], Limits) -> Result<String>`; `report::fit_pile(target, cfg, FitOpts{dry_run, accept, propose, progress, root}) -> Result<PileReport>` and `report::render_pile_text`; `commands::{fit_one_text, check_text}`; `spec::ParseSpec::json_schema()`; `config::SAMPLE_CONFIG`, `config::config_file_path()`.

- [ ] **Step 1: Write the failing tests**:

```rust
#[tokio::test]
async fn draft_prints_or_writes_and_never_overwrites() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".draft 2025-*.csv 2025-*.xlsx", None).await;
    assert!(o.ok, "{}", o.text);
    assert_eq!(o.echo, ".draft 2025-*.csv 2025-*.xlsx");
    assert!(o.text.contains("CREATE TABLE dataset") && o.text.contains("in 11 of 12 file(s)"));
    let Payload::Drafted { wrote, .. } = o.payload else { panic!() };
    assert!(wrote.is_none());

    let o = s.run(".draft 2025-*.csv --to mine.tdy.sql", None).await;
    assert!(o.ok && d.path().join("mine.tdy.sql").exists());
    assert!(o.text.contains("wrote mine.tdy.sql"));
    let o = s.run(".draft 2025-*.csv --to mine.tdy.sql", None).await;
    assert!(!o.ok && o.text.contains("exists"));

    let o = s.run(".draft nothing-*.csv", None).await;
    assert!(!o.ok && o.text.contains("no file matches"));
}

#[tokio::test]
async fn fit_reports_refusals_writes_no_lock_and_then_fits_the_fixed_target() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".fit sales.tdy.sql", None).await;
    assert!(!o.ok);
    let Payload::Fitted(r) = &o.payload else { panic!("{:?}", o.payload) };
    assert_eq!((r.fitted, r.failed), (9, 3));
    assert!(r.lock_written.is_none());
    assert!(o.text.contains("9 of 12 file(s) fit `sales`"));
    assert!(o.text.ends_with("Error: 3 file(s) cannot reach the declared schema; no lock written. Fix them, exclude them, or widen the target.\n"));
    assert!(!d.path().join("sales.tdy.lock").exists());

    std::fs::copy(corpus().join("sales_ok.tdy.sql"), d.path().join("sales_ok.tdy.sql")).unwrap();
    let o = s.run(".fit sales_ok.tdy.sql", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(d.path().join("sales_ok.tdy.lock").exists());

    // One file against the target: the fit-one text path.
    let o = s.run(".fit sales.tdy.sql 2025-07.csv", None).await;
    assert!(!o.ok && o.text.contains("cannot reach `sales`"));
    let o = s.run(".fit sales.tdy.sql 2025-01.csv --dry-run", None).await;
    assert!(o.ok && o.text.contains("--dry-run: nothing written"));
}

#[tokio::test]
async fn check_schema_config_edit() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".check sales.tdy.sql", None).await;
    assert!(o.ok && o.text.contains("nothing to check"));
    let o = s.run(".schema", None).await;
    assert!(o.ok && o.text.trim_start().starts_with('{'));
    let o = s.run(".config init", None).await;
    assert!(o.ok && o.text.contains("[backend]") || o.text.contains("backend"));
    let o = s.run(".edit sales.tdy.sql", None).await;
    assert!(o.ok);
    assert!(matches!(o.payload, Payload::Edit(ref p) if p.ends_with("sales.tdy.sql")));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test console draft fit check`
Expected: "not implemented yet".

- [ ] **Step 3: Implement** — add to `dispatch`:

```rust
            Command::Draft { files, to } => {
                let paths = self.expand(&files)?;
                let echo = format!(
                    ".draft {}{}",
                    paths.iter().map(|p| quote_rel(&self.display_rel(p))).collect::<Vec<_>>().join(" "),
                    to.as_ref().map(|t| format!(" --to {t}")).unwrap_or_default()
                );
                let ddl = crate::draft::draft_target(&paths, self.cfg.limits)?;
                let wrote = match to {
                    Some(t) => {
                        let dest = self.resolve(&t)?;
                        if dest.exists() {
                            bail!("{t} exists; choose another name or remove it first");
                        }
                        crate::fileio::atomic_write(&dest, &ddl)?;
                        Some(dest)
                    }
                    None => None,
                };
                let text = match &wrote {
                    Some(p) => format!("wrote {}\n", self.display_rel(p)),
                    None => ddl.clone(),
                };
                Outcome { echo, text, payload: Payload::Drafted { ddl, wrote }, ok: true }
            }
            Command::Fit { target, file: Some(file), dry_run, propose } => {
                let (t, f) = (self.resolve(&target)?, self.resolve(&file)?);
                let out = crate::commands::fit_one_text(&t, &f, &self.cfg, dry_run, propose, progress).await?;
                let mut text = out.text;
                if !out.ok {
                    let msg = if out.gaps { "no plan reaches the declared schema".to_string() }
                              else { format!("could not fit {}", file) };
                    let _ = write!(text, "Error: {msg}\n");
                }
                Outcome { echo: String::new(), text, payload: Payload::Nothing, ok: out.ok }
            }
            Command::Fit { target, file: None, dry_run, propose } => {
                let t = self.resolve(&target)?;
                self.fit_pile(&t, &[], dry_run, propose, progress).await?
            }
            Command::Check { target, against } => {
                let t = self.resolve(&target)?;
                let files = against.iter().map(|a| self.resolve(a)).collect::<Result<Vec<_>>>()?;
                let out = crate::commands::check_text(&t, &files, self.cfg.limits)?;
                let mut text = out.text;
                if !out.ok {
                    let _ = writeln!(text, "Error: {} file(s) do not produce the declared schema", out.bad);
                }
                Outcome { echo: String::new(), text, payload: Payload::Nothing, ok: out.ok }
            }
            Command::Schema => Outcome::ok(
                format!("{}\n", serde_json::to_string_pretty(&crate::spec::ParseSpec::json_schema())?),
                Payload::Nothing,
            ),
            Command::ConfigInit => {
                let path = crate::config::config_file_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.config/tdy/config.toml".into());
                Outcome::ok(format!("# write this to {path}\n\n{}\n", crate::config::SAMPLE_CONFIG), Payload::Nothing)
            }
            Command::Edit { file } => {
                let p = self.resolve(&file)?;
                Outcome::ok(String::new(), Payload::Edit(p))
            }
```

and the shared pile fit (also used by `.accept`'s second step in Task 9):

```rust
impl Session {
    async fn fit_pile(
        &mut self,
        target: &Path,
        accept: &[PathBuf],
        dry_run: bool,
        propose: bool,
        progress: Option<&progress::Sink>,
    ) -> Result<Outcome> {
        let r = crate::report::fit_pile(
            target,
            &self.cfg,
            crate::report::FitOpts {
                dry_run,
                accept,
                propose,
                progress: progress.cloned(),
                root: Some(&self.root),
            },
        )
        .await?;
        let mut text = crate::report::render_pile_text(&r);
        let ok = r.failed == 0;
        if !ok {
            let _ = writeln!(
                text,
                "Error: {} file(s) cannot reach the declared schema; no lock written. \
                 Fix them, exclude them, or widen the target.",
                r.failed
            );
        }
        Ok(Outcome { echo: String::new(), text, payload: Payload::Fitted(r), ok })
    }
}

fn quote_rel(s: &str) -> String {
    if s.chars().any(char::is_whitespace) { format!("{s:?}") } else { s.to_string() }
}
```

`progress.cloned()` — `Option<&Sink>` → `Option<Sink>` (`Sink` is an `Arc`, so cloning is a refcount bump). `fit_pile` with `root: Some(&self.root)` also confines every member the target's globs resolve to, the same way the MCP server does.

Check `config::SAMPLE_CONFIG` is `pub` (main.rs uses it via `config::SAMPLE_CONFIG`, so it is).

- [ ] **Step 4: Run the tests**

Run: `cargo test --test console`
Expected: all pass. The `.fit` failing-text assertion is the first half of the same-text promise; Task 12 closes it against the binary.

- [ ] **Step 5: Commit**

```bash
git add src/console/mod.rs tests/console.rs
git commit -m "console: .draft/.fit/.check/.schema/.config/.edit — the pile commands, refusals as outcomes"
```

---

### Task 8: SQL — multi-line assembly, `.output`, and the result table

**Files:**
- Modify: `src/console/mod.rs`, `src/provider.rs` (extract `format_table`), `docs/design/2026-09-01-console-and-workbench.md` §4 (one bullet)
- Test: `tests/console.rs`

**Interfaces:**
- Consumes: `provider::run_query_rooted(sql, cfg, frozen, root: Option<PathBuf>)`, `provider::write_output(schema, batches, OutputFormat, Option<&Path>)`, `OutputFormat::{parse, for_output_path, Table}`.
- Produces: `provider::format_table(batches: &[RecordBatch]) -> Result<String>` (the exact text `write_output`'s Table branch writes: `pretty_format_batches` + `\n`); `Session::sql_pending() -> bool`.

**Decision recorded here and in the spec:** the query context is **not** cached across commands. A `.sniff --force` or a file rewrite between two queries would otherwise serve a `MemTable` built from the old spec — a silently wrong answer, the one thing tdy forbids. Each SQL statement builds its own context, as the CLI does; caching within one statement (the same file named twice) is unchanged.

- [ ] **Step 1: Write the failing tests**:

```rust
#[tokio::test]
async fn sql_runs_when_the_statement_ends_and_spans_lines() {
    let d = pile();
    let mut s = session(d.path()).await;
    s.run(".sniff 2025-01.csv --no-llm", None).await;
    let o = s.run("SELECT count(*) AS n, sum(betrag) AS total", None).await;
    assert!(o.ok && matches!(o.payload, Payload::Continue) && s.sql_pending());
    let o = s.run("FROM messy('2025-01.csv');", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(!s.sql_pending());
    assert_eq!(o.echo, "SELECT count(*) AS n, sum(betrag) AS total\nFROM messy('2025-01.csv');");
    let Payload::Query(t) = o.payload else { panic!() };
    assert_eq!(t.columns, ["n", "total"]);
    assert_eq!(t.rows, [["4", "4460.00"]]);
    assert!(o.text.contains("| 4 ") && o.text.contains("4460.00"));

    // A dot-command discards a pending statement, out loud.
    s.run("SELECT 1", None).await;
    let o = s.run(".ls", None).await;
    assert!(o.ok && o.text.starts_with("note: discarded incomplete statement"));
    assert!(!s.sql_pending());

    // A bad statement is an error outcome, not a crash.
    let o = s.run("SELEKT 1;", None).await;
    assert!(!o.ok && o.text.starts_with("Error: "));
}

#[tokio::test]
async fn output_routes_the_next_result_to_a_file() {
    let d = pile();
    let mut s = session(d.path()).await;
    s.run(".sniff 2025-01.csv --no-llm", None).await;
    let o = s.run(".output jan.csv", None).await;
    assert!(o.ok && o.text.contains("next result -> jan.csv"));
    let o = s.run("SELECT region, betrag FROM messy('2025-01.csv') ORDER BY region;", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(o.text.contains("wrote 4 row(s) to jan.csv"));
    let written = std::fs::read_to_string(d.path().join("jan.csv")).unwrap();
    assert!(written.starts_with("region,betrag\n"));
    // The route is consumed.
    let o = s.run("SELECT 1 AS one;", None).await;
    assert!(o.text.contains("| one |"));
    // Refuses to overwrite without --force.
    let o = s.run(".output jan.csv", None).await;
    assert!(!o.ok && o.text.contains("exists"));
    assert!(s.run(".output jan.csv --force", None).await.ok);
    assert!(s.run(".output", None).await.ok);   // back to the screen
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test console sql output`
Expected: fail (non-dot lines are errors; `.output` unimplemented).

- [ ] **Step 3: Implement.** In `provider.rs`, extract from `write_output`'s Table branch:

```rust
/// The table the CLI prints for a result, as a string.
pub fn format_table(batches: &[RecordBatch]) -> Result<String> {
    let text = datafusion::arrow::util::pretty::pretty_format_batches(batches).context("formatting result")?;
    Ok(format!("{text}\n"))
}
```
and have the Table branch call it (`write!(w, "{}", format_table(batches)?)`), keeping the >10,000-rows stderr note where it is (it is CLI advice).

In `Session::run`, replace the parse-first logic with:

```rust
    pub async fn run(&mut self, line: &str, progress: Option<&progress::Sink>) -> Outcome {
        let trimmed = line.trim_end();
        let is_dot = trimmed.trim_start().starts_with('.');
        let mut prefix = String::new();
        if is_dot && !self.sql_buffer.is_empty() {
            prefix = format!("note: discarded incomplete statement: {}\n", first_line(&self.sql_buffer));
            self.sql_buffer.clear();
        }
        if !is_dot {
            if !self.sql_buffer.is_empty() {
                self.sql_buffer.push('\n');
            }
            self.sql_buffer.push_str(trimmed);
            if !self.sql_buffer.trim_end().ends_with(';') {
                return Outcome { echo: trimmed.to_string(), text: String::new(), payload: Payload::Continue, ok: true };
            }
            let sql = std::mem::take(&mut self.sql_buffer);
            let mut o = match self.run_sql(&sql).await {
                Ok(o) => o,
                Err(e) => Outcome::error(&sql, format!("{e:#}")),
            };
            o.echo = sql;
            return o;
        }
        let cmd = match parse(trimmed) { /* as before */ };
        let mut o = /* dispatch as before */;
        if !prefix.is_empty() {
            o.text = prefix + &o.text;
        }
        o
    }

    pub fn sql_pending(&self) -> bool { !self.sql_buffer.is_empty() }

    async fn run_sql(&mut self, sql: &str) -> Result<Outcome> {
        let (schema, batches) =
            crate::provider::run_query_rooted(sql, &self.cfg, false, Some(self.root.clone())).await?;
        let table = table_of(&schema, &batches, QUERY_ROWS_CAP);
        let text = match self.output.take() {
            Some((path, fmt)) => {
                crate::provider::write_output(&schema, &batches, fmt, Some(&path))?;
                format!("wrote {} row(s) to {}\n", table.total, self.display_rel(&path))
            }
            None => crate::provider::format_table(&batches)?,
        };
        Ok(Outcome { echo: String::new(), text, payload: Payload::Query(table), ok: true })
    }
```

with `const QUERY_ROWS_CAP: usize = 500;` and `fn first_line(s: &str) -> &str { s.lines().next().unwrap_or("") }`. `run_query_rooted` prints inference notes to **stderr** through its `report(...)` call; that is acceptable for the plain console and is slice 2's problem for the workbench (noted in the commit message).

`.output` in `dispatch`:

```rust
            Command::Output { file: None, .. } => {
                self.output = None;
                Outcome::ok("next result -> screen\n".into(), Payload::Nothing)
            }
            Command::Output { file: Some(f), format, force } => {
                let path = self.resolve(&f)?;
                if path.exists() && !force {
                    bail!("{f} exists; --force to overwrite");
                }
                let fmt = match format {
                    Some(x) => crate::provider::OutputFormat::parse(&x)?,
                    None => crate::provider::OutputFormat::for_output_path(&path)?,
                };
                self.output = Some((path, fmt));
                Outcome::ok(format!("next result -> {f}\n"), Payload::Nothing)
            }
```

- [ ] **Step 4: Amend the spec** — in `docs/design/2026-09-01-console-and-workbench.md` §4, replace the bullet "**`SessionContext` persists across lines** …" with:

> - **The query context is built per statement, not kept across lines.** A `.sniff --force` or an export overwritten between two queries would otherwise serve a `MemTable` built from the old spec — a silently wrong answer. Within one statement the same file named twice is still parsed once. Revisit if the provider's cache becomes fingerprint-keyed.

- [ ] **Step 5: Run the tests**

Run: `cargo test --test console && cargo test --workspace --lib --tests`
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add src/console/mod.rs src/provider.rs tests/console.rs docs/design/2026-09-01-console-and-workbench.md
git commit -m "console: SQL with sqlite's ';' rule, .output routing; no cross-statement cache (a stale MemTable is a wrong answer)"
```

---

### Task 9: `.accept` — two steps, evidence first

**Files:**
- Modify: `src/console/mod.rs`
- Test: `tests/console.rs`

**Interfaces:**
- Consumes: `evidence::for_spec(spec, path, limits, review, model_framed)`, `fit::review_reasons(&ParseSpec) -> Vec<String>`, `sidecar::load`, `Session::fit_pile` (Task 7).
- Produces: `Session::pending_accept() -> Option<(&Path, &str)>`; `pub fn render_evidence(rows: &[Evidence]) -> String`.

The member's spec must be *fresh* and *reviewable*: the sidecar written by the last `.fit` carries the review reasons (`fit::review_reasons`) and whether a model framed it (`provenance.method == Llm`). The Rappen file `2025-07.csv` needs a hand-written `decimal_shift` sidecar to be reviewable — `tests/fit.rs` has the manual-sidecar pattern; copy its TOML into the test.

- [ ] **Step 1: Write the failing test**:

```rust
const RAPPEN_SIDECAR: &str = r#"
# hand-written: Betrag Rp. is Rappen; shift the point two places left.
spec_version = 1
[source]
path = "2025-07.csv"
blake3 = "REPLACED"
bytes = 0
[provenance]
method = "manual"
tool_version = "test"
created_at = "2026-01-01T00:00:00Z"
[spec]
confidence = 1.0
notes = []
[spec.extraction]
format = "delimited"
delimiter = ";"
[[spec.transforms]]
op = "promote_header"
rows = 1
[[spec.columns]]
name = "month"
source = "Datum"
nullable = false
[spec.columns.dtype]
type = "date"
format = "%d.%m.%Y"
[[spec.columns]]
name = "region"
source = "Region"
nullable = false
[spec.columns.dtype]
type = "utf8"
[[spec.columns]]
name = "amount_chf"
source = "Betrag Rp."
nullable = false
[spec.columns.dtype]
type = "decimal"
precision = 14
scale = 2
[spec.columns.parse]
decimal_shift = -2
"#;

fn write_rappen_sidecar(dir: &Path) {
    let file = dir.join("2025-07.csv");
    let (hash, bytes) = tdy::sidecar::hash_file(&file).unwrap();
    let toml = RAPPEN_SIDECAR.replace("blake3 = \"REPLACED\"", &format!("blake3 = \"{hash}\""))
        .replace("bytes = 0", &format!("bytes = {bytes}"));
    std::fs::write(dir.join("2025-07.csv.tdy.toml"), toml).unwrap();
}

#[tokio::test]
async fn accept_shows_evidence_first_and_accepts_only_on_repeat() {
    let d = pile();
    write_rappen_sidecar(d.path());
    let mut s = session(d.path()).await;

    // The pile fit leaves 2025-07 waiting on review (manual spec, decimal_shift).
    let o = s.run(".fit sales.tdy.sql", None).await;
    let Payload::Fitted(r) = &o.payload else { panic!() };
    let m07 = r.members.iter().find(|m| m.path == "2025-07.csv").unwrap();
    assert!(m07.review.is_some() && !m07.accepted, "{m07:?}");

    // Step one: evidence, nothing written.
    let o = s.run(".accept sales.tdy.sql 2025-07.csv", None).await;
    assert!(o.ok, "{}", o.text);
    let Payload::Evidence { rows, .. } = &o.payload else { panic!("{:?}", o.payload) };
    assert!(!rows.is_empty());
    assert!(o.text.contains("170000") && o.text.contains("1700.00"));
    assert!(o.text.contains("run `.accept sales.tdy.sql 2025-07.csv` again to accept"));
    assert!(s.pending_accept().is_some());

    // Any other command in between resets to step one.
    s.run(".ls", None).await;
    assert!(s.pending_accept().is_none());
    let o = s.run(".accept sales.tdy.sql 2025-07.csv", None).await;
    assert!(matches!(o.payload, Payload::Evidence { .. }));

    // Step two: the same line again performs the acceptance.
    let o = s.run(".accept sales.tdy.sql 2025-07.csv", None).await;
    let Payload::Fitted(r) = &o.payload else { panic!("{:?}", o.payload) };
    let m07 = r.members.iter().find(|m| m.path == "2025-07.csv").unwrap();
    assert!(m07.accepted, "{m07:?}");
    assert!(s.pending_accept().is_none());

    // A member with nothing to review is refused, not silently accepted.
    let o = s.run(".accept sales.tdy.sql 2025-01.csv", None).await;
    assert!(!o.ok && o.text.contains("nothing to accept"));
}
```

If `fit_pile` treats a *manual* sidecar without `--accept` differently from what this test assumes (check `tests/fit.rs` for how the Rappen acceptance is exercised), align the test with the real behaviour — the properties that must hold are: step one writes nothing and returns evidence; step two is the only path to `accepted == true`; an unrelated command in between resets.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test console accept`
Expected: "not implemented yet".

- [ ] **Step 3: Implement.** In `run`, before dispatching any command **other than** `Accept`, clear `self.pending_accept = None` (put this right after `parse` succeeds: `if !matches!(cmd, Command::Accept { .. }) { self.pending_accept = None; }`). Then in `dispatch`:

```rust
            Command::Accept { target, member } => {
                let t = self.resolve(&target)?;
                let member_path = crate::fileio::confine(&crate::lockfile::target_dir(&t).join(&member), &self.root)
                    .map_err(|_| anyhow::anyhow!("{member}: outside the console's root"))?;
                let same = self.pending_accept.as_ref().map(|(pt, pm)| pt == &t && pm == &member).unwrap_or(false);
                if same {
                    self.pending_accept = None;
                    let mut o = self.fit_pile(&t, &[PathBuf::from(&member)], false, false, progress).await?;
                    o.text = format!("accepted {member}\n\n{}", o.text);
                    return Ok(o);
                }
                let sc = match crate::sidecar::load(&member_path)? {
                    crate::sidecar::SidecarStatus::Fresh(sc) => sc,
                    _ => bail!("{member} has no fresh sidecar; run `.fit {target}` first"),
                };
                let reasons = crate::fit::review_reasons(&sc.spec);
                if reasons.is_empty() {
                    bail!("nothing to accept: {member} has no judgement waiting on review");
                }
                let model_framed = matches!(sc.provenance.method, crate::spec::InferenceMethod::Llm);
                let rows = crate::evidence::for_spec(&sc.spec, &member_path, self.cfg.limits, &reasons.join("; "), model_framed)?;
                let mut text = format!("evidence for {member} (nothing written):\n");
                for r in &reasons {
                    let _ = writeln!(text, "  review: {r}");
                }
                text.push_str(&render_evidence(&rows));
                let _ = writeln!(text, "\nrun `.accept {target} {member}` again to accept");
                self.pending_accept = Some((t.clone(), member.clone()));
                Outcome::ok(text, Payload::Evidence { target: t, member, rows })
            }
```

`fit_pile`'s `accept` parameter is the member *as the report names it* (relative to the target's directory) — confirm against how `tdy-tui/src/main.rs::spawn_fit` builds `accept_paths` (`PathBuf::from(member)`); it does.

`render_evidence`:

```rust
pub fn render_evidence(rows: &[crate::evidence::Evidence]) -> String {
    use crate::evidence::Evidence;
    let mut s = String::new();
    for e in rows {
        let _ = writeln!(s, "\n  {}", e.headline());
        match e {
            Evidence::Shift { head, smallest, largest, .. } => {
                for p in head.iter().take(5) {
                    let _ = writeln!(s, "    row {:<6} {:>14}  ->  {}", p.row, p.raw, p.parsed);
                }
                if let Some(p) = smallest { let _ = writeln!(s, "    smallest  row {:<6} {:>14}  ->  {}", p.row, p.raw, p.parsed); }
                if let Some(p) = largest  { let _ = writeln!(s, "    largest   row {:<6} {:>14}  ->  {}", p.row, p.raw, p.parsed); }
            }
            Evidence::Frame { header, head, .. } => {
                let _ = writeln!(s, "    header: {}", header.join(" | "));
                for r in head.iter().take(5) { let _ = writeln!(s, "    {}", r.join(" | ")); }
            }
            Evidence::Constant { .. } | Evidence::Unillustrated { .. } => {}
        }
    }
    s
}
```

Add `pub fn pending_accept(&self) -> Option<(&Path, &str)>`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test console`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add src/console/mod.rs tests/console.rs
git commit -m "console: .accept is two steps — the evidence first, the same line again to accept, anything else resets"
```

---

### Task 10: `console::line` — the prompt's line editor as a state machine

Raw-mode input so Up/Down recall history; kept out of the REPL loop so it is unit-tested without a terminal. Adds `crossterm` to `tdy` (already built for `tdy-tui`, so the workspace pays nothing new; the published crate's tree grows by crossterm's — accepted in the spec over a readline crate).

**Files:**
- Modify: `Cargo.toml` (`crossterm = "0.29"` under `[dependencies]`, matching `tdy-tui/Cargo.toml`)
- Create: `src/console/line.rs`
- Modify: `src/console/mod.rs` (`pub mod line;`)

**Interfaces:**
- Produces:

```rust
pub struct LineEditor { /* buffer, cursor, history: Vec<String>, history_pos: Option<usize>, stash: String */ }
pub enum Edit {
    /// Redraw the line: (text, cursor position in chars).
    Redraw,
    /// Enter: the line is complete.
    Submit(String),
    /// Ctrl-C on a non-empty line: cleared. On an empty line: Interrupt.
    Cleared,
    Interrupt,
    /// Ctrl-D on an empty line.
    Eof,
    Nothing,
}
impl LineEditor {
    pub fn new(history: Vec<String>) -> LineEditor;
    pub fn key(&mut self, k: crossterm::event::KeyEvent) -> Edit;
    pub fn text(&self) -> &str;
    pub fn cursor(&self) -> usize;
    pub fn history(&self) -> &[String];
    /// Record a submitted line (skips empty and consecutive duplicates).
    pub fn remember(&mut self, line: &str);
}
```

- [ ] **Step 1: Write the failing tests** in `src/console/line.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode) -> KeyEvent { KeyEvent::new(code, KeyModifiers::NONE) }
    fn ctrl(c: char) -> KeyEvent { KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL) }
    fn type_str(ed: &mut LineEditor, s: &str) { for c in s.chars() { ed.key(k(KeyCode::Char(c))); } }

    #[test]
    fn typing_editing_and_submit() {
        let mut ed = LineEditor::new(vec![]);
        type_str(&mut ed, ".sniff a.csv");
        assert_eq!((ed.text(), ed.cursor()), (".sniff a.csv", 12));
        ed.key(k(KeyCode::Left)); ed.key(k(KeyCode::Left));
        ed.key(k(KeyCode::Backspace));
        assert_eq!(ed.text(), ".sniff a.sv");
        ed.key(k(KeyCode::Home)); ed.key(k(KeyCode::Delete));
        assert_eq!(ed.text(), "sniff a.sv");
        ed.key(k(KeyCode::End)); type_str(&mut ed, "!");
        assert!(matches!(ed.key(k(KeyCode::Enter)), Edit::Submit(s) if s == "sniff a.sv!"));
        assert_eq!(ed.text(), "");
    }

    #[test]
    fn history_recall_keeps_the_draft() {
        let mut ed = LineEditor::new(vec!["first".into(), "second".into()]);
        type_str(&mut ed, "draft");
        ed.key(k(KeyCode::Up));
        assert_eq!(ed.text(), "second");
        ed.key(k(KeyCode::Up));
        assert_eq!(ed.text(), "first");
        ed.key(k(KeyCode::Up));                 // past the oldest: stays
        assert_eq!(ed.text(), "first");
        ed.key(k(KeyCode::Down)); ed.key(k(KeyCode::Down));
        assert_eq!(ed.text(), "draft");        // the draft comes back
    }

    #[test]
    fn remember_skips_empty_and_duplicates() {
        let mut ed = LineEditor::new(vec![]);
        ed.remember(".ls"); ed.remember(".ls"); ed.remember(""); ed.remember(".help");
        assert_eq!(ed.history(), [".ls", ".help"]);
    }

    #[test]
    fn control_keys() {
        let mut ed = LineEditor::new(vec![]);
        assert!(matches!(ed.key(ctrl('d')), Edit::Eof));
        assert!(matches!(ed.key(ctrl('c')), Edit::Interrupt));
        type_str(&mut ed, "abc");
        assert!(matches!(ed.key(ctrl('c')), Edit::Cleared));
        assert_eq!(ed.text(), "");
        type_str(&mut ed, "abc");
        assert!(matches!(ed.key(ctrl('d')), Edit::Nothing));   // not EOF mid-line
        assert!(matches!(ed.key(ctrl('u')), Edit::Redraw));
        assert_eq!(ed.text(), "");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib console::line`
Expected: compile error.

- [ ] **Step 3: Implement `src/console/line.rs`**

```rust
//! The prompt's line editor, as a state machine: a key in, an [`Edit`] out.
//! No terminal in here, so every behaviour is a unit test. Deliberately
//! small — insert, delete, move, history — because history recall is the
//! feature that matters and a readline crate is a dependency tree.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub enum Edit { Redraw, Submit(String), Cleared, Interrupt, Eof, Nothing }

pub struct LineEditor {
    buf: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// Index into history while browsing; None = editing the draft.
    pos: Option<usize>,
    /// The draft, stashed while browsing history.
    stash: Vec<char>,
}

impl LineEditor {
    pub fn new(history: Vec<String>) -> LineEditor {
        LineEditor { buf: vec![], cursor: 0, history, pos: None, stash: vec![] }
    }
    pub fn text(&self) -> String { self.buf.iter().collect() }   // (return String; adjust the tests' `ed.text()` comparisons accordingly — `&str` comparisons against `String` work via `==`)
    pub fn cursor(&self) -> usize { self.cursor }
    pub fn history(&self) -> &[String] { &self.history }

    pub fn remember(&mut self, line: &str) {
        if line.trim().is_empty() || self.history.last().map(String::as_str) == Some(line) {
            return;
        }
        self.history.push(line.to_string());
    }

    pub fn key(&mut self, k: KeyEvent) -> Edit {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match (k.code, ctrl) {
            (KeyCode::Char('c'), true) => {
                if self.buf.is_empty() { return Edit::Interrupt; }
                self.reset();
                Edit::Cleared
            }
            (KeyCode::Char('d'), true) => if self.buf.is_empty() { Edit::Eof } else { Edit::Nothing },
            (KeyCode::Char('u'), true) => { self.reset(); Edit::Redraw }
            (KeyCode::Char('a'), true) | (KeyCode::Home, _) => { self.cursor = 0; Edit::Redraw }
            (KeyCode::Char('e'), true) | (KeyCode::End, _) => { self.cursor = self.buf.len(); Edit::Redraw }
            (KeyCode::Char(c), false) => { self.buf.insert(self.cursor, c); self.cursor += 1; Edit::Redraw }
            (KeyCode::Backspace, _) => {
                if self.cursor > 0 { self.cursor -= 1; self.buf.remove(self.cursor); }
                Edit::Redraw
            }
            (KeyCode::Delete, _) => {
                if self.cursor < self.buf.len() { self.buf.remove(self.cursor); }
                Edit::Redraw
            }
            (KeyCode::Left, _) => { self.cursor = self.cursor.saturating_sub(1); Edit::Redraw }
            (KeyCode::Right, _) => { self.cursor = (self.cursor + 1).min(self.buf.len()); Edit::Redraw }
            (KeyCode::Up, _) => { self.browse(-1); Edit::Redraw }
            (KeyCode::Down, _) => { self.browse(1); Edit::Redraw }
            (KeyCode::Enter, _) => {
                let line: String = self.buf.iter().collect();
                self.reset();
                Edit::Submit(line)
            }
            _ => Edit::Nothing,
        }
    }

    fn reset(&mut self) { self.buf.clear(); self.cursor = 0; self.pos = None; self.stash.clear(); }

    fn browse(&mut self, dir: i32) {
        if self.history.is_empty() { return; }
        let next = match (self.pos, dir) {
            (None, -1) => { self.stash = self.buf.clone(); Some(self.history.len() - 1) }
            (None, _) => None,
            (Some(0), -1) => Some(0),
            (Some(i), -1) => Some(i - 1),
            (Some(i), _) if i + 1 >= self.history.len() => None,
            (Some(i), _) => Some(i + 1),
        };
        self.pos = next;
        self.buf = match next {
            Some(i) => self.history[i].chars().collect(),
            None => self.stash.clone(),
        };
        self.cursor = self.buf.len();
    }
}
```

Add `pub mod line;` to `src/console/mod.rs` and `crossterm = "0.29"` to `Cargo.toml`. Run `cargo tree -d | head` afterwards to confirm no duplicate crossterm versions between `tdy` and `tdy-tui`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib console::line`
Expected: 4 pass.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/console/line.rs src/console/mod.rs
git commit -m "console::line: the prompt's editor as a state machine — history recall without a readline crate"
```

---

### Task 11: `console::repl` and the entry points — `tdy`, `tdy console`, `tdy < script`

**Files:**
- Create: `src/console/repl.rs`
- Modify: `src/console/mod.rs` (`pub mod repl;`), `src/main.rs` (`command: Option<Command>`, `Command::Console`, the no-argument dispatch)
- Create: `tests/repl.rs`

**Interfaces:**
- Consumes: `Session`, `LineEditor`, `Payload::{Edit, Quit, Continue}`.
- Produces:

```rust
/// Read lines from `input` to EOF, run each, write `text` to `out`.
/// Returns the exit code: 0, or 1 at the FIRST failing outcome (stops there).
pub async fn run_batch(session: &mut Session, input: impl BufRead, out: &mut impl Write) -> Result<i32>;
/// The TTY loop: raw-mode prompt, history file, `$EDITOR` for `.edit`.
pub async fn run_interactive(session: &mut Session) -> Result<()>;
pub fn history_path() -> Option<PathBuf>;      // dirs::data_dir()/tdy/history
pub fn load_history(limit: usize) -> Vec<String>;
pub fn append_history(line: &str);
```

- [ ] **Step 1: Write the failing tests** — `tests/repl.rs`:

```rust
//! `tdy` with piped stdin: the batch runner.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

fn pile() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(corpus()).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("2025-") && !n.ends_with(".tdy.toml") {
            std::fs::copy(e.path(), d.path().join(&n)).unwrap();
        }
    }
    d
}

fn run_script(dir: &Path, script: &str, args: &[&str]) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(args)
        .current_dir(dir)
        .env("TDY_BACKEND", "none")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

#[test]
fn piped_stdin_runs_lines_and_prints_text() {
    let d = pile();
    let (code, out, _) = run_script(
        d.path(),
        ".sniff 2025-01.csv --no-llm\nSELECT count(*) AS n\nFROM messy('2025-01.csv');\n.ls\n",
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("preview (heuristic method"));
    assert!(out.contains("| n |") && out.contains("| 4 |"));
    assert!(out.contains("2025-01.csv") && out.contains("sniffed"));
}

#[test]
fn batch_stops_at_the_first_error_with_exit_one() {
    let d = pile();
    let (code, out, _) = run_script(d.path(), ".nope\n.ls\n", &[]);
    assert_eq!(code, 1);
    assert!(out.contains("Error: unknown command `.nope`"));
    assert!(!out.contains("2025-01.csv"), "did not stop: {out}");
}

#[test]
fn tdy_console_subcommand_is_the_same_runner() {
    let d = pile();
    let (code, out, _) = run_script(d.path(), ".help\n", &["console"]);
    assert_eq!(code, 0);
    assert!(out.contains(".sniff FILE"));
}

#[test]
fn quit_ends_the_script_cleanly() {
    let d = pile();
    let (code, out, _) = run_script(d.path(), ".quit\n.ls\n", &[]);
    assert_eq!(code, 0);
    assert!(!out.contains("2025-01.csv"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test repl`
Expected: `tdy` with no subcommand exits 2 with clap's usage error.

- [ ] **Step 3: Implement `src/console/repl.rs`**

```rust
//! The plain console: `tdy>` on a TTY, a batch runner on a pipe.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{self, Event};
use crossterm::terminal;

use super::line::{Edit, LineEditor};
use super::{Payload, Session};

const HISTORY_LIMIT: usize = 1000;

pub fn history_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("tdy").join("history"))
}

pub fn load_history(limit: usize) -> Vec<String> {
    let Some(p) = history_path() else { return vec![] };
    let Ok(text) = std::fs::read_to_string(p) else { return vec![] };
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let skip = lines.len().saturating_sub(limit);
    lines.into_iter().skip(skip).collect()
}

pub fn append_history(line: &str) {
    let Some(p) = history_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        // One command per line; a multi-line statement is stored with its
        // newlines escaped so the file stays line-oriented.
        let _ = writeln!(f, "{}", line.replace('\n', "\\n"));
    }
}

pub async fn run_batch(session: &mut Session, input: impl BufRead, out: &mut impl Write) -> Result<i32> {
    for line in input.lines() {
        let line = line.context("reading input")?;
        let o = session.run(&line, None).await;
        write!(out, "{}", o.text)?;
        out.flush()?;
        if let Payload::Edit(p) = &o.payload {
            writeln!(out, "Error: no editor in batch mode; edit {} yourself", p.display())?;
            return Ok(1);
        }
        if !o.ok {
            return Ok(1);
        }
        if session.wants_quit() {
            break;
        }
    }
    Ok(0)
}

pub async fn run_interactive(session: &mut Session) -> Result<()> {
    let mut stdout = std::io::stdout();
    let mut ed = LineEditor::new(load_history(HISTORY_LIMIT));
    let sink = crate::progress::stderr_sink();
    loop {
        let prompt = if session.sql_pending() { "   -> " } else { "tdy> " };
        let line = match read_line(&mut ed, prompt, &mut stdout)? {
            Read::Line(l) => l,
            Read::Interrupt => {
                // Ctrl-C on an empty prompt abandons a pending statement.
                if session.sql_pending() {
                    let _ = session.run(".help nothing", None).await; // any dot-command discards it
                }
                continue;
            }
            Read::Eof => break,
        };
        let o = session.run(&line, Some(&sink)).await;
        if !o.echo.trim().is_empty() && !matches!(o.payload, Payload::Continue) {
            ed.remember(&o.echo);
            append_history(&o.echo);
        }
        print!("{}", o.text);
        stdout.flush()?;
        if let Payload::Edit(p) = &o.payload {
            run_editor(p)?;
        }
        if session.wants_quit() {
            break;
        }
    }
    Ok(())
}

enum Read { Line(String), Interrupt, Eof }

/// One line in raw mode, redrawing on every edit. Restores cooked mode on
/// every exit path, including `?`.
fn read_line(ed: &mut LineEditor, prompt: &str, out: &mut std::io::Stdout) -> Result<Read> {
    struct Raw;
    impl Drop for Raw { fn drop(&mut self) { let _ = terminal::disable_raw_mode(); } }
    terminal::enable_raw_mode()?;
    let _guard = Raw;
    let redraw = |ed: &LineEditor, out: &mut std::io::Stdout| -> Result<()> {
        // \r, clear line, prompt + text, then move the cursor back.
        let text = ed.text();
        let back = text.chars().count() - ed.cursor();
        write!(out, "\r\x1b[2K{prompt}{text}")?;
        if back > 0 { write!(out, "\x1b[{back}D")?; }
        out.flush()?;
        Ok(())
    };
    redraw(ed, out)?;
    loop {
        if let Event::Key(k) = event::read()? {
            if k.kind != event::KeyEventKind::Press { continue; }
            match ed.key(k) {
                Edit::Redraw | Edit::Nothing => redraw(ed, out)?,
                Edit::Cleared => { write!(out, "^C\r\n")?; redraw(ed, out)?; }
                Edit::Interrupt => { write!(out, "^C\r\n")?; out.flush()?; return Ok(Read::Interrupt); }
                Edit::Eof => { write!(out, "\r\n")?; out.flush()?; return Ok(Read::Eof); }
                Edit::Submit(l) => { write!(out, "\r\n")?; out.flush()?; return Ok(Read::Line(l)); }
            }
        }
    }
}

fn run_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(path).status()
        .with_context(|| format!("cannot run editor {editor}"))?;
    if !status.success() {
        anyhow::bail!("{editor} exited with {status}");
    }
    println!("edited {}", path.display());
    Ok(())
}

/// Whether both ends are terminals — the interactive console's precondition.
pub fn stdio_is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}
```

The `Read::Interrupt` handling that abandons a pending statement is clumsy (`.help nothing`); add a proper `pub fn discard_pending(&mut self) -> Option<String>` to `Session` and call that instead — it returns the discarded text so the loop can print the same "note: discarded incomplete statement" line.

Add `pub mod repl;` to `src/console/mod.rs`.

- [ ] **Step 4: The entry points in `src/main.rs`**

```rust
struct Cli {
    /// With no subcommand: the console (or the workbench, if `tdy-tui` is installed and this is a terminal).
    #[command(subcommand)]
    command: Option<Command>,
    …
}

enum Command {
    …
    /// The plain console, always (even when the workbench is installed).
    Console,
    …
}
```

In `run()`:

```rust
    let command = match cli.command {
        Some(c) => c,
        None => {
            if tdy::console::repl::stdio_is_tty() && workbench_on_path() {
                return exec_workbench(None);
            }
            if tdy::console::repl::stdio_is_tty() && !workbench_on_path() {
                eprintln!("terminal UI not installed: cargo install --path tdy-tui");
            }
            Command::Console
        }
    };
    match command {
        Command::Console => {
            let cfg = config::load(&overrides)?;
            let mut session = tdy::console::Session::new(std::path::Path::new("."), cfg)?;
            if tdy::console::repl::stdio_is_tty() {
                tdy::console::repl::run_interactive(&mut session).await?;
            } else {
                let stdin = std::io::stdin();
                let mut stdout = std::io::stdout();
                let code = tdy::console::repl::run_batch(&mut session, stdin.lock(), &mut stdout).await?;
                if code != 0 {
                    std::process::exit(code);
                }
            }
        }
        …
    }
```

with

```rust
fn workbench_on_path() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("tdy-tui").is_file()))
        .unwrap_or(false)
}

/// Hand the terminal to `tdy-tui`, exactly as `tdy ui` does.
fn exec_workbench(target: Option<PathBuf>) -> Result<()> {
    let mut cmd = std::process::Command::new("tdy-tui");
    if let Some(t) = target { cmd.arg(t); }
    match cmd.status() {
        Ok(st) => std::process::exit(st.code().unwrap_or(1)),
        Err(e) => anyhow::bail!("cannot run tdy-tui: {e}"),
    }
}
```

and make the existing `Command::Ui { target }` arm call `exec_workbench(target)` after its not-found check (keep the existing hint text for the not-found case). The `--json` global flag has no meaning for the console; ignore it.

- [ ] **Step 5: Run the tests**

Run: `cargo test --test repl && cargo test --workspace --lib --tests`
Expected: green. Then try it by hand once, in a terminal: `cargo run -q -- console` from `testdata/drifting_exports` — type `.ls`, Up-arrow, `.quit`; confirm the prompt redraws and the terminal is sane afterwards (`reset` if not, and fix the drop guard). Clean up: `rm -f testdata/drifting_exports/*.tdy.toml`.

- [ ] **Step 6: Commit**

```bash
git add src/console/repl.rs src/console/mod.rs src/main.rs tests/repl.rs
git commit -m "tdy alone opens the console (the workbench when installed); piped stdin is a batch runner"
```

---

### Task 12: The same-text promise, asserted against the binary

**Files:**
- Test: `tests/console.rs`

- [ ] **Step 1: Write the tests**:

```rust
#[tokio::test]
async fn fit_text_equals_the_binary_stdout_plus_its_error_line() {
    let d = pile();
    let cli = tdy(d.path(), &["fit", "sales.tdy.sql"]);
    assert!(!cli.status.success());
    let stderr = String::from_utf8_lossy(&cli.stderr);
    let error_line = stderr.lines().find(|l| l.starts_with("Error: ")).expect("an Error line");
    let expected = format!("{}{error_line}\n", String::from_utf8_lossy(&cli.stdout));
    // Fresh copy so sidecars written by the CLI run do not change the text.
    let d2 = pile();
    let mut s = session(d2.path()).await;
    let o = s.run(".fit sales.tdy.sql", None).await;
    assert_eq!(o.text, expected);
}

#[tokio::test]
async fn query_text_equals_the_binary() {
    let d = pile();
    tdy(d.path(), &["sniff", "2025-01.csv", "--no-llm"]);
    let cli = tdy(d.path(), &["query", "SELECT region, betrag FROM messy('2025-01.csv') ORDER BY region"]);
    assert!(cli.status.success());
    let mut s = session(d.path()).await;
    let o = s.run("SELECT region, betrag FROM messy('2025-01.csv') ORDER BY region;", None).await;
    assert_eq!(o.text, String::from_utf8_lossy(&cli.stdout));
}

#[tokio::test]
async fn draft_text_equals_the_binary() {
    let d = pile();
    let cli = tdy(d.path(), &["draft", "2025-01.csv", "2025-02.csv", "2025-12.csv"]);
    let mut s = session(d.path()).await;
    let o = s.run(".draft 2025-01.csv 2025-02.csv 2025-12.csv", None).await;
    assert_eq!(o.text, String::from_utf8_lossy(&cli.stdout));
}
```

Note the sniff test in Task 4 already covers `.sniff`; `check` is covered there too. If the `.fit` comparison fails only on the member *paths* (the console passes an absolute target path, the CLI a relative one, and `render_pile_text` prints `target_file`), normalise by running the CLI with the same absolute path — the point is that the renderer is shared, not that path spelling is.

- [ ] **Step 2: Run**

Run: `cargo test --test console`
Expected: green. If `fit_text_…` differs by a trailing newline, fix the *console* side to match the binary exactly (the binary is the contract).

- [ ] **Step 3: Commit**

```bash
git add tests/console.rs
git commit -m "tests: the console's text for fit, query and draft is the binary's, byte for byte"
```

---

### Task 13: README, CLAUDE.md, and the quick start on the console

**Files:**
- Modify: `README.md` (new "## The console" section before "## Commands"; the Quick start's steps 1–2 switch to console lines; the `tdy ui` section mentions `tdy` alone), `CLAUDE.md` (a paragraph on `console`, `commands`, the moved `evidence`, the crossterm dependency, the test count)

- [ ] **Step 1: README — "The console" section**, inserted immediately before `## Commands`:

````markdown
## The console

`tdy` with nothing after it opens a console, the way `sqlite3` does. SQL
runs as typed; a statement ends with `;` and may span lines. Everything else
is a dot-command, one per CLI subcommand, with the CLI's flags:

```
tdy> .ls
2025-01.csv    sniffed 0.95 (heuristic)
2025-02.csv
sales.tdy.sql  target, no lock
tdy> .sniff 2025-02.csv
tdy> SELECT region, sum(betrag) FROM messy('2025-02.csv') GROUP BY 1;
tdy> .draft 2025-*.csv 2025-*.xlsx --to sales.tdy.sql
tdy> .fit sales.tdy.sql
tdy> .accept sales.tdy.sql 2025-07.csv      # shows the evidence; again to accept
tdy> .output totals.parquet
tdy> SELECT region, sum(amount_chf) FROM dataset('sales.tdy.sql') GROUP BY 1;
```

`.help` lists them. Globs are expanded by the console itself; every path is
confined to the directory the console was started in. The text a command
prints is the same text the subcommand prints — one function produces both,
and a test holds them equal — so nothing you learn in one place is wrong in
the other.

Piped input makes it a batch runner: `tdy < setup.tdy` runs the lines and
exits non-zero at the first error. `tdy console` forces the plain console;
when the terminal UI is installed, `tdy` alone opens that instead.
````

- [ ] **Step 2: README — the quick start.** Step 1 becomes: `tdy` (opens the console) then `.sniff 2025-01.csv --no-llm` and the SQL statement with a trailing `;`, outputs unchanged. Step 2's `tdy draft …` becomes `.draft 2025-*.csv 2025-*.xlsx`; the heredoc that writes `sales.tdy.sql` stays a shell heredoc **outside** the console (the console has no way to write a file from a literal, and that is fine — say "in your shell"); then `.fit sales.tdy.sql`, the second heredoc, `.fit` again, and the dataset query with `;`. Keep every output block byte-identical to a real run: re-run the sequence in a scratch copy with `tdy < script` and paste from that.

- [ ] **Step 3: CLAUDE.md** — under "The terminal UI is `tdy-tui`…", add a paragraph:

> **The console is `src/console/`** (`parse` — pure grammar; `Session::run` — one line in, an `Outcome { echo, text, payload, ok }` out; `line` — the prompt's editor as a state machine; `repl` — the TTY loop and the piped batch runner). `tdy` with no subcommand opens it (or execs `tdy-tui` when that is on PATH and stdio is a terminal). Its `text` is the CLI's text because `src/commands.rs` produces both — the CLI arms print what `commands::*_text` return, and `tests/console.rs` asserts the console's `.fit`/`.sniff`/`.draft`/query text equals the binary's. The query context is deliberately **not** kept across statements (a re-sniff between two queries would serve a stale `MemTable`). `.accept` is two steps in the session itself (`pending_accept`), and any other command in between resets it. `evidence` now lives in the library (`src/evidence.rs`); `tdy-tui` re-exports it.

Update the test count line in "Commands" after running the suite.

- [ ] **Step 4: Verify the quick start by executing it** — extract every `bash` block of the Quick start into a script under the scratchpad (skip clone/install), run it from a clean copy under a scratch `HOME`, and confirm the printed numbers match the README (4 / 4460.00 / 2025-01-31; 9 of 12 then 9 of 9; the four regional totals). Console lines inside the quick start are run by piping them: `tdy <<'EOF' … EOF`.

- [ ] **Step 5: Full suite and commit**

Run: `cargo test --workspace --lib --tests`
Expected: green.

```bash
git add README.md CLAUDE.md
git commit -m "README: the console; quick start runs through it. CLAUDE.md: console, commands, evidence's new home"
```

---

## Self-review against the spec

- **§3 grammar:** every row of both tables has a `Command` variant and a `dispatch` arm (Tasks 2, 5–9). `.help [CMD]`, `.quit`/`.exit`, Ctrl-D — Tasks 5, 10, 11. Globs — Task 1/5. Quoting — Task 2. Selection-as-implicit-argument is slice 2 (the `Missing` variant is its hook).
- **§4 dispatcher:** `parse`, `Session`, `Outcome`/`Payload` — Tasks 2, 5. Non-printing — Global Constraints + Task 4. Multi-line `;` — Task 8. Progress `Sink` — Tasks 7, 9, 11. Errors as outcomes — Task 5. Context caching — deliberately changed in Task 8 and written back into the spec.
- **§5 entry points:** `tdy` alone / not-a-TTY / `tdy console` / `tdy ui` — Task 11. History file — Task 11 (`dirs::data_dir()` is `~/.local/share` on Linux, matching the spec).
- **§8 review gate:** two-step `.accept`, reset on any other command, no multi-member, no skip flag — Tasks 2 (`--yes` is an unknown flag) and 9. `.edit` note about the stale lock is slice 2's browser status; the plain console prints "edited …" — acceptable, noted.
- **§9 safety:** confinement (Task 5, and `root: Some(&self.root)` in `fit_pile`, `run_query_rooted` with root), `.draft --to` / `.output` overwrite rules (Tasks 7, 8), `.sniff` fresh-kept note (Task 4).
- **§10 tests:** `tests/console.rs` (Tasks 4–9, 12), `tests/repl.rs` (Task 11), editor state machine (Task 10). The render tests are slice 2.
- **Type consistency:** `Outcome { echo, text, payload, ok }` everywhere; `Payload::Fitted(PileReport)` used by `.fit` and `.accept` step two; `Table` used by `Sniffed.preview` and `Query`; `CheckOutcome { text, ok, bad }` and `FitOneOutcome { text, ok, wrote, gaps }` as extended in Task 4 step 4; `Session::fit_pile(&Path, &[PathBuf], bool, bool, Option<&Sink>)` in Tasks 7 and 9.
- **Placeholders:** the two `todo!()` in Task 4 are explicitly "replace in this step" with the source location of the bodies to move; nothing else is deferred.
