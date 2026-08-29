//! Recognising the two shapes that are not tables: log lines and
//! column-aligned reports.
//!
//! Without this, `backend = "none"` — the default, the one where nothing
//! leaves your machine — could only ever see a log file as a badly delimited
//! CSV. The heuristics here are deliberately narrow: they either recognise a
//! well-known shape with high confidence or decline, leaving the file to the
//! delimited sniffer or the LLM tier. A wrong guess here is worse than no
//! guess, because it would silently drop every line that does not match.

use crate::spec::FixedField;

/// A recognised line-oriented format.
pub struct LinePattern {
    /// Regex with named capture groups, one per column.
    pub pattern: String,
    /// Matched lines as a share of matched-plus-unexplained lines.
    pub match_rate: f32,
    /// Human name, for the spec's notes.
    pub name: &'static str,
    /// Sampled lines that became records.
    pub records: usize,
    /// Sampled lines that will be dropped (continuations, banners).
    pub skipped: usize,
}

/// Well-known log layouts, most specific first. Each must use named groups
/// only, because the group names become the column names.
const LOG_PATTERNS: &[(&str, &str)] = &[
    (
        "nginx/apache combined",
        r#"^(?P<remote_addr>\S+) (?P<ident>\S+) (?P<remote_user>\S+) \[(?P<time_local>[^\]]+)\] "(?P<request>[^"]*)" (?P<status>\d{3}) (?P<body_bytes>\S+) "(?P<referer>[^"]*)" "(?P<user_agent>[^"]*)".*$"#,
    ),
    (
        "nginx/apache common",
        r#"^(?P<remote_addr>\S+) (?P<ident>\S+) (?P<remote_user>\S+) \[(?P<time_local>[^\]]+)\] "(?P<request>[^"]*)" (?P<status>\d{3}) (?P<body_bytes>\S+)\s*$"#,
    ),
    (
        "ISO-timestamped application log",
        r"^(?P<ts>\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?)\s+\[?(?P<level>TRACE|DEBUG|INFO|WARN|WARNING|ERROR|FATAL|CRITICAL)\]?\s+(?P<message>.*)$",
    ),
    (
        "syslog",
        r"^(?P<ts>[A-Z][a-z]{2}\s+\d{1,2} \d{2}:\d{2}:\d{2}) (?P<host>\S+) (?P<process>[^:\[\s]+)(?:\[(?P<pid>\d+)\])?: (?P<message>.*)$",
    ),
    (
        "bracketed-level log",
        r"^\[(?P<ts>[^\]]+)\]\s*\[(?P<level>[A-Za-z]+)\]\s*(?P<message>.*)$",
    ),
];

/// Minimum share of *records* (as opposed to continuation lines) a pattern
/// must match to be believed. Logs legitimately contain banners and stack
/// traces, but a real match is overwhelming, not marginal.
const MIN_MATCH_RATE: f32 = 0.75;
/// A file where only a handful of lines are records is not a log.
const MIN_RECORD_SHARE: f32 = 0.25;

/// Try to recognise `text` as a known log format.
///
/// A line that does not match is not automatically evidence against the
/// pattern: a Java stack trace is a dozen lines *belonging to* the record
/// above it. Those are counted as continuations, not as failures — but every
/// one of them will be dropped at extraction time, so the count is reported
/// and the caller says so in the spec's notes.
pub fn detect_log_pattern(text: &str) -> Option<LinePattern> {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .take(400)
        .collect();
    if lines.len() < 3 {
        return None;
    }

    let mut best: Option<LinePattern> = None;
    for (name, pat) in LOG_PATTERNS {
        let Ok(re) = regex::Regex::new(pat) else { continue };
        let matched: Vec<bool> = lines.iter().map(|l| re.is_match(l)).collect();
        let hits = matched.iter().filter(|m| **m).count();
        if hits < 3 {
            continue;
        }
        // Walk the file deciding, for each non-matching line, whether it
        // belongs to the record above it (indented, or following a line that
        // was itself part of a record) or is an unexplained orphan.
        let mut orphans = 0usize;
        let mut attached_to_record = false;
        for (i, line) in lines.iter().enumerate() {
            if matched[i] {
                attached_to_record = true;
                continue;
            }
            let indented = line.starts_with(char::is_whitespace);
            if indented || attached_to_record {
                continue; // a continuation of the record above
            }
            orphans += 1;
        }
        let rate = hits as f32 / (hits + orphans) as f32;
        let share = hits as f32 / lines.len() as f32;
        if rate >= MIN_MATCH_RATE
            && share >= MIN_RECORD_SHARE
            && best.as_ref().map(|b| rate > b.match_rate).unwrap_or(true)
        {
            best = Some(LinePattern {
                pattern: (*pat).to_string(),
                match_rate: rate,
                name,
                records: hits,
                skipped: lines.len() - hits,
            });
        }
    }
    best
}

/// Try to recognise `text` as a column-aligned fixed-width report.
///
/// The signal is a run of character positions that is blank in essentially
/// every line: that is a gutter between two fields. Anything less
/// clear-cut — varying line lengths, fewer than two gutters, too few lines —
/// is declined.
pub fn detect_fixed_width(text: &str) -> Option<Vec<FixedField>> {
    let lines: Vec<Vec<char>> = text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .take(400)
        .map(|l| l.chars().collect())
        .collect();
    if lines.len() < 4 {
        return None;
    }

    let width = lines.iter().map(|l| l.len()).max().unwrap_or(0);
    if width < 8 || width > 4096 {
        return None;
    }
    // Reject anything that looks delimited: a fixed-width report has no
    // consistent separator character doing the work.
    let has_delim = |l: &Vec<char>| l.contains(&',') || l.contains(&';') || l.contains(&'\t');
    if lines.iter().all(has_delim) {
        return None;
    }

    // A position is a gutter if it is blank (or past end-of-line) everywhere.
    let is_gutter: Vec<bool> = (0..width)
        .map(|c| lines.iter().all(|l| l.get(c).map(|ch| *ch == ' ').unwrap_or(true)))
        .collect();

    // Fields are the maximal runs of non-gutter positions, but a single space
    // inside a text column is common, so require gutters of >= 2 columns to
    // split. (Two spaces is the conventional minimum in aligned reports.)
    let mut fields: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut gutter_run = 0usize;
    for c in 0..width {
        if is_gutter[c] {
            gutter_run += 1;
            if gutter_run >= 2 {
                if let Some(s) = start.take() {
                    fields.push((s, c + 1 - gutter_run));
                }
            }
        } else {
            gutter_run = 0;
            if start.is_none() {
                start = Some(c);
            }
        }
    }
    if let Some(s) = start {
        fields.push((s, width));
    }

    if fields.len() < 2 || fields.len() > 64 {
        return None;
    }
    // Every field must actually carry content in most lines; a report where a
    // "field" is empty everywhere is a misread.
    let populated = |(s, e): (usize, usize)| {
        let n = lines
            .iter()
            .filter(|l| l.get(s..e.min(l.len())).map(|sl| sl.iter().any(|c| !c.is_whitespace())).unwrap_or(false))
            .count();
        n as f32 / lines.len() as f32 >= 0.5
    };
    if !fields.iter().copied().all(populated) {
        return None;
    }

    // Names: use the first line if it reads like a header (no field is a bare
    // number), else generate them.
    let header_names: Option<Vec<String>> = {
        let first = &lines[0];
        let cells: Vec<String> = fields
            .iter()
            .map(|(s, e)| {
                first
                    .get(*s..(*e).min(first.len()))
                    .map(|sl| sl.iter().collect::<String>().trim().to_string())
                    .unwrap_or_default()
            })
            .collect();
        let all_named = cells.iter().all(|c| {
            !c.is_empty() && c.parse::<f64>().is_err() && c.chars().any(|ch| ch.is_alphabetic())
        });
        if all_named {
            Some(cells)
        } else {
            None
        }
    };

    let names: Vec<String> = match header_names {
        Some(n) => n,
        None => (1..=fields.len()).map(|i| format!("col_{i}")).collect(),
    };

    Some(
        fields
            .iter()
            .zip(names)
            .map(|((s, e), name)| FixedField { name, start: *s as u32, end: *e as u32 })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NGINX: &str = r#"192.168.1.1 - - [05/Jan/2026:10:00:01 +0100] "GET /a HTTP/1.1" 200 1234 "-" "curl/8.0"
10.0.0.7 - alice [05/Jan/2026:10:00:02 +0100] "POST /b HTTP/1.1" 201 55 "https://x/" "Mozilla/5.0"
10.0.0.8 - - [05/Jan/2026:10:00:03 +0100] "GET /c HTTP/1.1" 404 0 "-" "Mozilla/5.0"
10.0.0.9 - - [05/Jan/2026:10:00:04 +0100] "GET /d HTTP/1.1" 500 12 "-" "Go-http-client/2.0"
"#;

    #[test]
    fn recognises_nginx_combined() {
        let p = detect_log_pattern(NGINX).unwrap();
        assert_eq!(p.name, "nginx/apache combined");
        assert!(p.match_rate > 0.99);
        let re = regex::Regex::new(&p.pattern).unwrap();
        let caps = re.captures("10.0.0.7 - alice [05/Jan/2026:10:00:02 +0100] \"POST /b HTTP/1.1\" 201 55 \"https://x/\" \"Mozilla/5.0\"").unwrap();
        assert_eq!(caps.name("status").unwrap().as_str(), "201");
        assert_eq!(caps.name("remote_user").unwrap().as_str(), "alice");
    }

    #[test]
    fn recognises_iso_application_log() {
        let log = "2026-01-05 10:00:01 INFO  login user=um\n\
                   2026-01-05 10:00:07 WARN  slow query 1200ms\n\
                   2026-01-05 10:01:00 ERROR db timeout\n\
                   2026-01-05 10:01:02 DEBUG retrying\n";
        let p = detect_log_pattern(log).unwrap();
        let re = regex::Regex::new(&p.pattern).unwrap();
        let c = re.captures("2026-01-05 10:01:00 ERROR db timeout").unwrap();
        assert_eq!(c.name("level").unwrap().as_str(), "ERROR");
        assert_eq!(c.name("message").unwrap().as_str(), "db timeout");
    }

    #[test]
    fn recognises_syslog() {
        let log = "Jan  5 10:00:01 host sshd[123]: Accepted publickey\n\
                   Jan  5 10:00:02 host cron: session opened\n\
                   Jan  5 10:00:03 host sshd[124]: Disconnected\n\
                   Jan  5 10:00:04 host kernel: usb disconnect\n";
        let p = detect_log_pattern(log).unwrap();
        assert_eq!(p.name, "syslog");
    }

    #[test]
    fn declines_a_csv() {
        let csv = "a,b,c\n1,2,3\n4,5,6\n7,8,9\n";
        assert!(detect_log_pattern(csv).is_none());
        assert!(detect_fixed_width(csv).is_none());
    }

    #[test]
    fn declines_a_mostly_unmatched_log() {
        let mixed = "2026-01-05 10:00:01 INFO ok\n\
                     random line one\n\
                     random line two\n\
                     random line three\n";
        assert!(detect_log_pattern(mixed).is_none());
    }

    #[test]
    fn stack_traces_are_continuations_not_evidence_against_the_pattern() {
        let log = "2026-02-10 03:14:02,117 INFO  [main] starting\n\
                   2026-02-10 03:14:03,004 DEBUG [main] pool created\n\
                   2026-02-10 03:14:12,003 ERROR [pool-2] job failed\n\
                   java.lang.IllegalStateException: pool exhausted\n\
                   \tat com.acme.db.Pool.borrow(Pool.java:214)\n\
                   \tat com.acme.batch.JobRunner.run(JobRunner.java:88)\n\
                   Caused by: java.net.SocketTimeoutException: read timed out\n\
                   \tat java.base/java.net.Socket.read(Socket.java:1)\n\
                   2026-02-10 03:15:00,000 INFO  [main] retrying\n";
        let p = detect_log_pattern(log).expect("a Java log is a log");
        assert_eq!(p.records, 4);
        assert_eq!(p.skipped, 5);
        assert!(p.match_rate > 0.99, "continuation lines must not count against it");
    }

    #[test]
    fn a_few_matching_lines_in_a_non_log_are_not_enough() {
        // Three timestamped lines buried in prose: not a log format.
        let mut text = String::new();
        for i in 0..3 {
            text.push_str(&format!("2026-01-0{} 10:00:00 INFO x\n", i + 1));
        }
        for i in 0..40 {
            text.push_str(&format!("Some unrelated prose line number {i}.\n"));
        }
        assert!(detect_log_pattern(&text).is_none());
    }

    #[test]
    fn finds_fixed_width_fields() {
        let report = "\
NAME       AMOUNT  CITY
Mueller       100  Bern
Meier        2000  Zug
Rossi         -50  Lugano
Keller       1234  Basel
";
        let fields = detect_fixed_width(report).unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].name, "NAME");
        assert_eq!(fields[1].name, "AMOUNT");
        assert_eq!(fields[2].name, "CITY");
        // The name field must not swallow the amount column.
        assert!(fields[0].end <= fields[1].start);
    }

    #[test]
    fn fixed_width_without_a_header_generates_names() {
        let report = "\
Mueller       100
Meier        2000
Rossi         -50
Keller       1234
";
        let fields = detect_fixed_width(report).unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "col_1");
    }

    #[test]
    fn fixed_width_declines_free_text() {
        let prose = "The quick brown fox\njumps over the lazy dog\nand then keeps going\nfor a while longer\n";
        // Free prose has no stable gutters; at most it yields one field.
        assert!(detect_fixed_width(prose).map(|f| f.len() < 2).unwrap_or(true));
    }

    #[test]
    fn fixed_width_handles_non_ascii_by_character_position() {
        let report = "\
NAME       AMOUNT
Müller        100
Özil          200
Grün          300
";
        let fields = detect_fixed_width(report).unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].start, 0);
        // Character positions, not byte positions: the umlaut must not shift
        // the AMOUNT column.
        assert!(fields[1].start >= 10 && fields[1].start <= 12, "{:?}", fields[1]);
    }
}
