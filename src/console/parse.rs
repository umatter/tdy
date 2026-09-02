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
    /// Discard a half-typed SQL statement (console-only — a workbench
    /// Ctrl-C dispatches this rather than a bespoke `WbAction`, so the
    /// plain REPL gains it for free too).
    Abort,
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
        "abort" => {
            Args::collect("abort", args, &[], &[])?.exactly(&[])?;
            Command::Abort
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
        assert_eq!(p(".abort"), Command::Abort);
        assert_eq!(
            parse(".abort now"),
            Err(ParseError::Unexpected { command: "abort", token: "now".into() })
        );
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
