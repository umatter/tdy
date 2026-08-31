//! Finding `messy('path')` references in SQL text, correctly.
//!
//! The async pre-pass has to know which files a query will touch *before*
//! DataFusion plans it, because spec inference is async and
//! `TableFunctionImpl::call` is not. A regex over the raw SQL gets this wrong
//! in both directions: it finds `messy('x')` inside a `--` comment or inside
//! a string literal (and then infers a spec for, or errors on, a file the
//! query never reads), and it mis-reads paths containing an escaped quote.
//!
//! So we tokenize just enough SQL to be right: comments, single-quoted
//! literals with `''` escapes, and quoted identifiers.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessyRef {
    pub path: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(char),
}

/// If a `$` at `at` opens a dollar-quoted string, return (tag length
/// including both `$`, index just past it). `$1` (a placeholder) is not one.
fn dollar_tag(b: &[char], at: usize) -> Option<(usize, usize)> {
    let mut j = at + 1;
    while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') && !b[j].is_ascii_digit() {
        j += 1;
    }
    if b.get(j) == Some(&'$') {
        Some((j + 1 - at, j + 1))
    } else {
        None
    }
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let b: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        // Line comment
        if c == '-' && b.get(i + 1) == Some(&'-') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Block comment. Deliberately non-nesting, matching the dialect
        // DataFusion parses with: if the two disagreed about where a comment
        // ends, the pre-pass and the planner would disagree about which files
        // the query reads.
        if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i < b.len() && !(b[i] == '*' && b.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        // Dollar-quoted string ($$...$$ or $tag$...$tag$). Rare, but an
        // apostrophe inside one would otherwise open a literal that swallows
        // every later messy() reference.
        if c == '$' {
            if let Some((tag_len, body_start)) = dollar_tag(&b, i) {
                let close: Vec<char> = b[i..i + tag_len].to_vec();
                let mut j = body_start;
                let mut found = None;
                while j + close.len() <= b.len() {
                    if b[j..j + close.len()] == close[..] {
                        found = Some(j);
                        break;
                    }
                    j += 1;
                }
                let end = found.map(|f| f + close.len()).unwrap_or(b.len());
                out.push(Tok::Str(b[body_start..found.unwrap_or(b.len())].iter().collect()));
                i = end;
                continue;
            }
        }
        // Single-quoted string literal, '' is an escaped quote
        if c == '\'' {
            i += 1;
            let mut s = String::new();
            loop {
                if i >= b.len() {
                    break; // unterminated literal: take what we have
                }
                if b[i] == '\'' {
                    if b.get(i + 1) == Some(&'\'') {
                        s.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                s.push(b[i]);
                i += 1;
            }
            out.push(Tok::Str(s));
            continue;
        }
        // Quoted identifier: not a path, but must not be mistaken for one
        if c == '"' || c == '`' {
            let close = c;
            i += 1;
            let mut s = String::new();
            while i < b.len() && b[i] != close {
                s.push(b[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Ident(s));
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            let mut s = String::new();
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                s.push(b[i]);
                i += 1;
            }
            out.push(Tok::Ident(s));
            continue;
        }
        if !c.is_whitespace() {
            out.push(Tok::Punct(c));
        }
        i += 1;
    }
    out
}

/// Every `messy('path'[, 'hint'])` call in `sql`, in order of appearance,
/// de-duplicated by path (the first hint for a path wins).
/// Paths passed as the first argument to `dataset('…')`.
///
/// Same tokenizer, same reason: a `dataset()` inside a comment or a string
/// literal is not a reference to anything.
pub fn find_dataset_refs(sql: &str) -> Vec<String> {
    let toks = tokenize(sql);
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let is_it = matches!(&toks[i], Tok::Ident(id) if id.eq_ignore_ascii_case("dataset"));
        if !is_it || toks.get(i + 1) != Some(&Tok::Punct('(')) {
            i += 1;
            continue;
        }
        if let Some(Tok::Str(p)) = toks.get(i + 2) {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
        i += 3;
    }
    out
}

pub fn find_messy_refs(sql: &str) -> Vec<MessyRef> {
    let toks = tokenize(sql);
    let mut out: Vec<MessyRef> = Vec::new();
    let mut i = 0usize;
    while i < toks.len() {
        let is_messy = matches!(&toks[i], Tok::Ident(id) if id.eq_ignore_ascii_case("messy"));
        if !is_messy {
            i += 1;
            continue;
        }
        if toks.get(i + 1) != Some(&Tok::Punct('(')) {
            i += 1;
            continue;
        }
        let path = match toks.get(i + 2) {
            Some(Tok::Str(s)) => s.clone(),
            // messy(<not a literal>) — the UDTF will produce the error; the
            // pre-pass just has nothing to prepare.
            _ => {
                i += 2;
                continue;
            }
        };
        let hint = match (toks.get(i + 3), toks.get(i + 4)) {
            (Some(Tok::Punct(',')), Some(Tok::Str(h))) => Some(h.clone()),
            _ => None,
        };
        if !out.iter().any(|r| r.path == path) {
            out.push(MessyRef { path, hint });
        }
        i += 3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(sql: &str) -> Vec<String> {
        find_messy_refs(sql).into_iter().map(|r| r.path).collect()
    }

    #[test]
    fn plain_reference() {
        assert_eq!(paths("SELECT * FROM messy('a.csv')"), vec!["a.csv"]);
    }

    #[test]
    fn case_and_spacing() {
        assert_eq!(paths("select * from MESSY  (  'a.csv'  )"), vec!["a.csv"]);
    }

    #[test]
    fn hint_is_captured() {
        let r = find_messy_refs("SELECT * FROM messy('s.log', 'nginx access log')");
        assert_eq!(r[0].hint.as_deref(), Some("nginx access log"));
    }

    #[test]
    fn line_comment_is_not_a_reference() {
        assert_eq!(
            paths("SELECT * FROM messy('real.csv') -- messy('ghost.csv')"),
            vec!["real.csv"]
        );
    }

    #[test]
    fn block_comment_is_not_a_reference() {
        assert_eq!(
            paths("SELECT /* messy('ghost.csv') */ * FROM messy('real.csv')"),
            vec!["real.csv"]
        );
    }

    #[test]
    fn block_comments_do_not_nest_because_the_sql_parser_does_not() {
        // Whatever this is, DataFusion will reject it; what matters is that
        // the scanner ends the comment where the planner does.
        assert_eq!(
            paths("SELECT /* outer /* inner */ messy('x.csv') */ 1"),
            vec!["x.csv"]
        );
    }

    #[test]
    fn string_literal_is_not_a_reference() {
        assert_eq!(
            paths("SELECT * FROM messy('real.csv') WHERE note = 'see messy(''ghost.csv'')'"),
            vec!["real.csv"]
        );
    }

    #[test]
    fn escaped_quote_in_path() {
        // A file literally named  it's.csv
        assert_eq!(paths("SELECT * FROM messy('it''s.csv')"), vec!["it's.csv"]);
    }

    #[test]
    fn several_files_deduped_in_order() {
        assert_eq!(
            paths("SELECT * FROM messy('a.csv') JOIN messy('b.csv') USING (k) WHERE 1 IN (SELECT 1 FROM messy('a.csv'))"),
            vec!["a.csv", "b.csv"]
        );
    }

    #[test]
    fn quoted_identifier_is_not_a_path() {
        assert_eq!(paths(r#"SELECT "messy" FROM messy('a.csv')"#), vec!["a.csv"]);
    }

    #[test]
    fn identifier_prefix_does_not_match() {
        assert!(paths("SELECT * FROM messydata('a.csv')").is_empty());
        assert!(paths("SELECT * FROM not_messy('a.csv')").is_empty());
    }

    #[test]
    fn non_literal_argument_is_skipped() {
        assert!(paths("SELECT * FROM messy(some_col)").is_empty());
    }

    #[test]
    fn unterminated_literal_does_not_hang() {
        assert_eq!(paths("SELECT * FROM messy('a.csv"), vec!["a.csv"]);
    }

    #[test]
    fn a_dollar_quoted_string_does_not_swallow_the_query() {
        // The apostrophe inside $$...$$ must not open a literal.
        assert_eq!(
            paths("SELECT $$it's fine$$ AS note, x FROM messy('real.csv')"),
            vec!["real.csv"]
        );
        assert_eq!(
            paths("SELECT $tag$don't$tag$, x FROM messy('real.csv')"),
            vec!["real.csv"]
        );
        // A placeholder is not a dollar quote.
        assert_eq!(paths("SELECT * FROM messy('a.csv') WHERE x = $1"), vec!["a.csv"]);
    }

    #[test]
    fn windows_path_survives() {
        assert_eq!(
            paths(r"SELECT * FROM messy('C:\data\Q1 2025\umsatz.xlsx')"),
            vec![r"C:\data\Q1 2025\umsatz.xlsx"]
        );
    }
}
