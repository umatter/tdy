//! tdy's reader for JSON *data*: a document that remembers the order its keys
//! were written in, and the digits its numbers were written with.
//!
//! `serde_json::Value` cannot do either. It keeps an object's keys sorted
//! (tdy builds it without `preserve_order`, and turning that on would reorder
//! every map in the dependency tree that shares the crate), and it holds a
//! number as a `u64`, an `i64` or an `f64` — so `12345678901234567890123`
//! came out as `1.2345678901234568e22` and a thirty-digit amount lost its
//! tail, silently. Its `arbitrary_precision` feature keeps the text but
//! changes how every `Value` number serialises through any other serializer
//! (a number written to TOML became a private table), so it was tried and
//! reverted. serde_json stays for tdy's *own* JSON (`--json`, MCP, the JSON
//! Schema); every file tdy reads as data goes through [`Node::parse`].
//!
//! The reader is a strict RFC 8259 recursive-descent parser over a `&str`.
//! It refuses what `serde_json` refuses, in serde_json's words where it can
//! (so a refusal reads the same as it did), with the same nesting limit (128)
//! and the same "is this the end of the input" classification
//! ([`ParseError::is_eof`]) the NDJSON truncated-last-line diagnosis rests on.
//!
//! **A number cell keeps its digits.** [`render_number`] is the whole rule: a
//! number that serde_json held exactly renders exactly as serde_json renders
//! it (`1.0`, `1e3` → `1000.0`, `-0` → `-0.0`), so ordinary data reads byte
//! for byte as it always did; a number it did not hold exactly renders as the
//! text the file wrote. Only the numbers that used to come out wrong change.

use std::fmt;

/// One JSON value, objects in document order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Null,
    Bool(bool),
    /// A number, as its cell renders it ([`render_number`]).
    Number(Num),
    String(String),
    Array(Vec<Node>),
    /// Keys in the order the document wrote them. A key written twice keeps
    /// its first position and its last value, as `serde_json` keeps the last.
    Object(Vec<(String, Node)>),
}

/// A JSON number: the text its cell holds, and whether that is the source's
/// own text because the double serde_json would have produced does not
/// denote the number the file wrote.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Num {
    text: String,
    #[cfg_attr(not(test), allow(dead_code))]
    verbatim: bool,
}

/// How deep arrays and objects may nest: serde_json's own limit, so a
/// document one refuses the other refuses too.
const MAX_DEPTH: usize = 128;

/// A malformed document, with where.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParseError {
    msg: &'static str,
    /// 1-based line and column, counted as serde_json counts them (bytes).
    line: usize,
    column: usize,
    eof: bool,
}

impl ParseError {
    /// The input ended inside a value — a truncated document, rather than a
    /// broken one.
    pub(crate) fn is_eof(&self) -> bool {
        self.eof
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} at line {} column {}", self.msg, self.line, self.column)
    }
}

impl std::error::Error for ParseError {}

impl Node {
    pub(crate) fn parse(text: &str) -> Result<Node, ParseError> {
        let mut p = Parser { src: text.as_bytes(), text, pos: 0 };
        p.skip_ws();
        let v = p.value(0)?;
        p.skip_ws();
        if p.pos < p.src.len() {
            return Err(p.err_at(p.pos, "trailing characters", false));
        }
        Ok(v)
    }

    /// RFC 6901, as `serde_json::Value::pointer` reads it: `""` is the whole
    /// document, `~1` is `/`, `~0` is `~`, and a token indexes an array only
    /// when it is a plain decimal index.
    pub(crate) fn pointer(&self, ptr: &str) -> Option<&Node> {
        if ptr.is_empty() {
            return Some(self);
        }
        let rest = ptr.strip_prefix('/')?;
        let mut at = self;
        for raw in rest.split('/') {
            let token: std::borrow::Cow<str> = if raw.contains('~') {
                raw.replace("~1", "/").replace("~0", "~").into()
            } else {
                raw.into()
            };
            at = match at {
                Node::Object(entries) => entries.iter().find(|(k, _)| *k == token).map(|(_, v)| v)?,
                Node::Array(items) => {
                    let plain = !token.is_empty()
                        && token.bytes().all(|b| b.is_ascii_digit())
                        && (token == "0" || !token.starts_with('0'));
                    if !plain {
                        return None;
                    }
                    items.get(token.parse::<usize>().ok()?)?
                }
                _ => return None,
            };
        }
        Some(at)
    }

    /// [`Node::pointer`], taking the value out rather than borrowing it.
    pub(crate) fn into_pointer(self, ptr: &str) -> Option<Node> {
        if ptr.is_empty() {
            return Some(self);
        }
        let rest = ptr.strip_prefix('/')?;
        let mut at = self;
        for raw in rest.split('/') {
            let token = raw.replace("~1", "/").replace("~0", "~");
            at = match at {
                Node::Object(entries) => entries.into_iter().find(|(k, _)| *k == token).map(|(_, v)| v)?,
                Node::Array(items) => {
                    let plain = !token.is_empty()
                        && token.bytes().all(|b| b.is_ascii_digit())
                        && (token == "0" || !token.starts_with('0'));
                    if !plain {
                        return None;
                    }
                    items.into_iter().nth(token.parse::<usize>().ok()?)?
                }
                _ => return None,
            };
        }
        Some(at)
    }

    pub(crate) fn is_object(&self) -> bool {
        matches!(self, Node::Object(_))
    }

    /// An object's entries in the order `serde_json::Value` iterates them —
    /// sorted by key — for the walks and headers that have always followed
    /// that order. Empty for anything but an object.
    pub(crate) fn sorted_entries(&self) -> Vec<&(String, Node)> {
        let Node::Object(entries) = self else { return Vec::new() };
        let mut v: Vec<&(String, Node)> = entries.iter().collect();
        v.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// "an object", "an array", "a number", … — for messages.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Node::Object(_) => "an object",
            Node::Array(_) => "an array",
            Node::Null => "null",
            Node::Bool(_) => "a boolean",
            Node::Number(_) => "a number",
            Node::String(_) => "a string",
        }
    }

    /// The value as a cell holds it: null is empty, a string is itself, a
    /// number is [`render_number`]'s text, and an array or object is compact
    /// JSON text ([`Node::to_json`]).
    pub(crate) fn cell(&self) -> String {
        match self {
            Node::Null => String::new(),
            Node::Bool(b) => b.to_string(),
            Node::Number(n) => n.text.clone(),
            Node::String(s) => s.clone(),
            nested => nested.to_json(),
        }
    }

    /// [`Node::cell`], moving a string or number's text rather than copying it.
    pub(crate) fn into_cell(self) -> String {
        match self {
            Node::Number(n) => n.text,
            Node::String(s) => s,
            other => other.cell(),
        }
    }

    /// Compact JSON text, written exactly as `serde_json::to_string` writes
    /// the same value — keys sorted, its escapes — except that a number is
    /// its cell text, so a nested number keeps its digits too.
    pub(crate) fn to_json(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    fn write_json(&self, out: &mut String) {
        match self {
            Node::Null => out.push_str("null"),
            Node::Bool(true) => out.push_str("true"),
            Node::Bool(false) => out.push_str("false"),
            Node::Number(n) => out.push_str(&n.text),
            Node::String(s) => write_json_string(s, out),
            Node::Array(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_json(out);
                }
                out.push(']');
            }
            Node::Object(_) => {
                out.push('{');
                for (i, (k, v)) in self.sorted_entries().into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }
}

/// A string as serde_json's compact formatter writes it: `"` and `\` and the
/// control characters escaped (the short forms where JSON has one, `\u00XX`
/// otherwise), everything else — `/`, DEL, non-ASCII — as itself.
fn write_json_string(s: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &str = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0x00..=0x1f => "",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        if esc.is_empty() {
            out.push_str("\\u00");
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        } else {
            out.push_str(esc);
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

/// The cell text of a number whose source text is `src` (already checked
/// against the RFC 8259 grammar), and whether it is that source text
/// verbatim.
///
/// serde_json holds an integer that fits `u64`/`i64` as one, and anything
/// else as the nearest `f64`. When that value is the number the file wrote,
/// the cell is serde_json's rendering of it — what tdy has always produced,
/// so ordinary data does not move by a byte. When it is not — an integer past
/// `u64`, a decimal with more digits than a double carries, a magnitude a
/// double overflows or underflows — the cell is the file's own text, and the
/// typing downstream decides what it is (an oversized integer stays text or
/// fits a declared `DECIMAL`; a long decimal parses exactly into one, or is
/// refused under the rounding rule).
///
/// "Is the number the file wrote" is decided on decimal digits, never on
/// floats: both texts are reduced to sign, significant digits and decimal
/// exponent, and compared. An integer written without a fraction or exponent
/// that serde_json could only hold as a double is never exact here, even when
/// the double happens to equal it: `100000000000000000000` is an identifier
/// as often as a quantity, and `1e20` is not what the file said. The one
/// exception is `-0`, which serde_json holds as `-0.0`, a zero either way.
pub(crate) fn render_number(src: &str) -> (String, bool) {
    let digits = src.strip_prefix('-').unwrap_or(src);
    let integer_syntax = digits.bytes().all(|b| b.is_ascii_digit());
    // At most 18 digits always fits i64/u64, and serde_json prints such an
    // integer as the digits the grammar already made canonical — except -0.
    if integer_syntax && digits.len() <= 18 && src != "-0" {
        return (src.to_string(), false);
    }
    let Ok(n) = serde_json::from_str::<serde_json::Number>(src) else {
        // Out of a double's range: serde_json refused the whole document.
        return (src.to_string(), true);
    };
    if n.is_u64() || n.is_i64() {
        return (n.to_string(), false);
    }
    if integer_syntax && digits.bytes().any(|b| b != b'0') {
        return (src.to_string(), true);
    }
    let rendered = n.to_string();
    if same_decimal(src, &rendered) {
        (rendered, false)
    } else {
        (src.to_string(), true)
    }
}

/// A decimal number reduced to what it denotes: sign, significant digits
/// (no leading or trailing zeros) and the decimal exponent of the first of
/// them. Zero has no digits, and is zero whatever its sign.
#[derive(Debug, PartialEq)]
struct Decimal {
    negative: bool,
    digits: Vec<u8>,
    exponent: i64,
}

/// [`Decimal`] of a JSON number's text or of a float rendering
/// (`1.5e-7`, `1e+22`, `1000.0`); `None` for anything else.
fn decimal_of(s: &str) -> Option<Decimal> {
    let (negative, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (mantissa, exp) = match rest.find(['e', 'E']) {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let (int, frac) = match mantissa.find('.') {
        Some(i) => (&mantissa[..i], &mantissa[i + 1..]),
        None => (mantissa, ""),
    };
    if int.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Saturate: an exponent past ±10^15 is no number a double renders, and
    // the comparison only has to come out unequal.
    const CAP: i64 = 1_000_000_000_000_000;
    let exponent: i64 = match exp {
        None => 0,
        Some(e) => {
            let (neg, d) = match e.as_bytes().first() {
                Some(b'-') => (true, &e[1..]),
                Some(b'+') => (false, &e[1..]),
                _ => (false, e),
            };
            if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mag = d.bytes().fold(0i64, |acc, b| (acc * 10 + i64::from(b - b'0')).min(CAP));
            if neg { -mag } else { mag }
        }
    };
    let all: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
    let lead = all.iter().take_while(|&&b| b == b'0').count();
    let mut digits = all[lead..].to_vec();
    while digits.last() == Some(&b'0') {
        digits.pop();
    }
    if digits.is_empty() {
        return Some(Decimal { negative: false, digits, exponent: 0 });
    }
    Some(Decimal { negative, digits, exponent: int.len() as i64 - lead as i64 + exponent })
}

/// Whether two decimal texts denote the same real number.
fn same_decimal(a: &str, b: &str) -> bool {
    match (decimal_of(a), decimal_of(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

struct Parser<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    /// An error at byte `at`, placed as serde_json places one: on the line
    /// and column of that byte, or of the last byte at end of input.
    fn err_at(&self, at: usize, msg: &'static str, eof: bool) -> ParseError {
        let i = (at + 1).min(self.src.len());
        let start_of_line = self.src[..i].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
        let line = 1 + self.src[..start_of_line].iter().filter(|&&b| b == b'\n').count();
        ParseError { msg, line, column: i - start_of_line, eof }
    }

    fn eof(&self, msg: &'static str) -> ParseError {
        self.err_at(self.src.len(), msg, true)
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.src.get(self.pos) {
            if matches!(b, b' ' | b'\n' | b'\t' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn value(&mut self, depth: usize) -> Result<Node, ParseError> {
        let Some(&b) = self.src.get(self.pos) else {
            return Err(self.eof("EOF while parsing a value"));
        };
        match b {
            b'n' => self.ident(b"null", Node::Null),
            b't' => self.ident(b"true", Node::Bool(true)),
            b'f' => self.ident(b"false", Node::Bool(false)),
            b'"' => {
                self.pos += 1;
                Ok(Node::String(self.string()?))
            }
            b'-' | b'0'..=b'9' => self.number(),
            b'[' => {
                if depth + 1 >= MAX_DEPTH {
                    return Err(self.err_at(self.pos, "recursion limit exceeded", false));
                }
                self.pos += 1;
                self.array(depth + 1)
            }
            b'{' => {
                if depth + 1 >= MAX_DEPTH {
                    return Err(self.err_at(self.pos, "recursion limit exceeded", false));
                }
                self.pos += 1;
                self.object(depth + 1)
            }
            _ => Err(self.err_at(self.pos, "expected value", false)),
        }
    }

    fn ident(&mut self, word: &'static [u8], node: Node) -> Result<Node, ParseError> {
        for &w in word {
            match self.src.get(self.pos) {
                None => return Err(self.eof("EOF while parsing a value")),
                Some(&b) if b == w => self.pos += 1,
                Some(_) => return Err(self.err_at(self.pos, "expected ident", false)),
            }
        }
        Ok(node)
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.src.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    /// One digit is required here: EOF or anything else is named as
    /// serde_json names it.
    fn need_digit(&self) -> Result<(), ParseError> {
        match self.src.get(self.pos) {
            Some(b'0'..=b'9') => Ok(()),
            None => Err(self.eof("EOF while parsing a value")),
            Some(_) => Err(self.err_at(self.pos, "invalid number", false)),
        }
    }

    fn number(&mut self) -> Result<Node, ParseError> {
        let start = self.pos;
        if self.src[self.pos] == b'-' {
            self.pos += 1;
        }
        self.need_digit()?;
        if self.src[self.pos] == b'0' {
            self.pos += 1;
            if matches!(self.src.get(self.pos), Some(b'0'..=b'9')) {
                return Err(self.err_at(self.pos, "invalid number", false));
            }
        } else {
            self.digits();
        }
        if self.src.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            self.need_digit()?;
            self.digits();
        }
        if matches!(self.src.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.src.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            self.need_digit()?;
            self.digits();
        }
        let (text, verbatim) = render_number(&self.text[start..self.pos]);
        Ok(Node::Number(Num { text, verbatim }))
    }

    /// A string's contents; the opening quote is already consumed.
    fn string(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        // Fast path: no escape before the closing quote, one copy.
        loop {
            match self.src.get(self.pos) {
                None => return Err(self.eof("EOF while parsing a string")),
                Some(b'"') => {
                    let s = self.text[start..self.pos].to_string();
                    self.pos += 1;
                    return Ok(s);
                }
                Some(b'\\') => break,
                Some(&b) if b < 0x20 => {
                    return Err(self.err_at(
                        self.pos,
                        "control character (\\u0000-\\u001F) found while parsing a string",
                        false,
                    ))
                }
                Some(_) => self.pos += 1,
            }
        }
        let mut out = String::with_capacity(self.pos - start + 16);
        out.push_str(&self.text[start..self.pos]);
        let mut run = self.pos;
        loop {
            match self.src.get(self.pos) {
                None => return Err(self.eof("EOF while parsing a string")),
                Some(b'"') => {
                    out.push_str(&self.text[run..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    out.push_str(&self.text[run..self.pos]);
                    self.pos += 1;
                    self.escape(&mut out)?;
                    run = self.pos;
                }
                Some(&b) if b < 0x20 => {
                    return Err(self.err_at(
                        self.pos,
                        "control character (\\u0000-\\u001F) found while parsing a string",
                        false,
                    ))
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// One escape; the backslash is already consumed.
    fn escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        let Some(&c) = self.src.get(self.pos) else {
            return Err(self.eof("EOF while parsing a string"));
        };
        self.pos += 1;
        match c {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\x08'),
            b'f' => out.push('\x0c'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let n = self.hex4()?;
                if (0xDC00..=0xDFFF).contains(&n) {
                    return Err(self.err_at(self.pos - 1, "lone leading surrogate in hex escape", false));
                }
                if !(0xD800..=0xDBFF).contains(&n) {
                    out.push(char::from_u32(u32::from(n)).expect("not a surrogate"));
                    return Ok(());
                }
                // A leading surrogate: the trailing one must follow at once.
                for want in [b'\\', b'u'] {
                    match self.src.get(self.pos) {
                        None => return Err(self.eof("EOF while parsing a string")),
                        Some(&b) if b == want => self.pos += 1,
                        Some(_) => {
                            return Err(self.err_at(self.pos, "unexpected end of hex escape", false));
                        }
                    }
                }
                let n2 = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&n2) {
                    return Err(self.err_at(self.pos - 1, "lone leading surrogate in hex escape", false));
                }
                let c = ((u32::from(n - 0xD800) << 10) | u32::from(n2 - 0xDC00)) + 0x1_0000;
                out.push(char::from_u32(c).expect("a paired surrogate is a scalar value"));
            }
            _ => return Err(self.err_at(self.pos - 1, "invalid escape", false)),
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u16, ParseError> {
        if self.pos + 4 > self.src.len() {
            self.pos = self.src.len();
            return Err(self.eof("EOF while parsing a string"));
        }
        let src = self.src;
        let mut n: u16 = 0;
        for &b in &src[self.pos..self.pos + 4] {
            let d = match b {
                b @ b'0'..=b'9' => b - b'0',
                b @ b'a'..=b'f' => b - b'a' + 10,
                b @ b'A'..=b'F' => b - b'A' + 10,
                _ => {
                    self.pos += 4;
                    return Err(self.err_at(self.pos - 1, "invalid escape", false));
                }
            };
            n = n * 16 + u16::from(d);
        }
        self.pos += 4;
        Ok(n)
    }

    fn array(&mut self, depth: usize) -> Result<Node, ParseError> {
        let mut items = Vec::new();
        self.skip_ws();
        match self.src.get(self.pos) {
            None => return Err(self.eof("EOF while parsing a list")),
            Some(b']') => {
                self.pos += 1;
                return Ok(Node::Array(items));
            }
            Some(_) => {}
        }
        loop {
            items.push(self.value(depth)?);
            self.skip_ws();
            match self.src.get(self.pos) {
                None => return Err(self.eof("EOF while parsing a list")),
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Node::Array(items));
                }
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                    match self.src.get(self.pos) {
                        None => return Err(self.eof("EOF while parsing a value")),
                        Some(b']') => return Err(self.err_at(self.pos, "trailing comma", false)),
                        Some(_) => {}
                    }
                }
                Some(_) => return Err(self.err_at(self.pos, "expected `,` or `]`", false)),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Node, ParseError> {
        // A duplicate keeps its first position and takes the last value. Up
        // to a few keys a scan is cheapest; past that a key-to-slot map, as a
        // scan of the entries so far made a 200,000-key object take minutes.
        const SCAN: usize = 16;
        let mut entries: Vec<(String, Node)> = Vec::new();
        let mut slot: Option<std::collections::HashMap<String, usize>> = None;
        self.skip_ws();
        match self.src.get(self.pos) {
            None => return Err(self.eof("EOF while parsing an object")),
            Some(b'}') => {
                self.pos += 1;
                return Ok(Node::Object(entries));
            }
            Some(_) => {}
        }
        loop {
            match self.src.get(self.pos) {
                None => return Err(self.eof("EOF while parsing a value")),
                Some(b'"') => self.pos += 1,
                Some(_) => return Err(self.err_at(self.pos, "key must be a string", false)),
            }
            let key = self.string()?;
            self.skip_ws();
            match self.src.get(self.pos) {
                None => return Err(self.eof("EOF while parsing an object")),
                Some(b':') => self.pos += 1,
                Some(_) => return Err(self.err_at(self.pos, "expected `:`", false)),
            }
            self.skip_ws();
            let v = self.value(depth)?;
            let existing = match &slot {
                Some(map) => map.get(&key).copied(),
                None => entries.iter().position(|(k, _)| *k == key),
            };
            match existing {
                Some(i) => entries[i].1 = v,
                None => {
                    if let Some(map) = &mut slot {
                        map.insert(key.clone(), entries.len());
                    }
                    entries.push((key, v));
                    if slot.is_none() && entries.len() > SCAN {
                        slot = Some(entries.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect());
                    }
                }
            }
            self.skip_ws();
            match self.src.get(self.pos) {
                None => return Err(self.eof("EOF while parsing an object")),
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Node::Object(entries));
                }
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.src.get(self.pos) == Some(&b'}') {
                        return Err(self.err_at(self.pos, "trailing comma", false));
                    }
                }
                Some(_) => return Err(self.err_at(self.pos, "expected `,` or `}`", false)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(text: &str) -> Node {
        Node::Number(Num { text: text.into(), verbatim: false })
    }

    #[test]
    fn keys_keep_the_documents_order_and_values_render_as_serde_json_does() {
        let n = Node::parse(r#"{"z":1,"a":{"y":2.5,"b":[true,null]},"m":"x","z":3}"#).unwrap();
        let Node::Object(e) = &n else { panic!() };
        let keys: Vec<&str> = e.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["z", "a", "m"]);
        assert_eq!(n.pointer("/z"), Some(&num("3")), "the last value wins");
        let text = r#"{"z":1,"a":{"y":2.5,"b":[true,null]},"m":"x"}"#;
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(Node::parse(text).unwrap().to_json(), serde_json::to_string(&v).unwrap());
    }

    /// The duplicate-key check must not scan the keys so far: a 200,000-key
    /// object took 342 s that way.
    #[test]
    fn a_wide_object_parses_in_linear_time() {
        let mut text = String::from(r#"{"id":1,"blob":{"#);
        for i in 0..200_000 {
            if i > 0 {
                text.push(',');
            }
            text.push_str(&format!("\"k{i}\":{i}"));
        }
        text.push_str(r#","k5":"again"}}"#);
        let t = std::time::Instant::now();
        let n = Node::parse(&text).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
        assert_eq!(n.pointer("/blob/k199999"), Some(&num("199999")));
        assert_eq!(n.pointer("/blob/k5"), Some(&Node::String("again".into())), "a late duplicate still wins");
        let Some(Node::Object(blob)) = n.pointer("/blob") else { panic!() };
        assert_eq!(blob.len(), 200_000);
        assert_eq!(blob[5].0, "k5", "and keeps its first position");
    }

    #[test]
    fn pointers_read_as_rfc_6901_says() {
        let n = Node::parse(r#"{"a/b":{"~k":[10,20]},"":1}"#).unwrap();
        assert_eq!(n.pointer("/a~1b/~0k/1"), Some(&num("20")));
        assert_eq!(n.pointer("/a~1b/~0k/01"), None);
        assert_eq!(n.pointer("/"), Some(&num("1")));
        assert_eq!(n.pointer(""), Some(&n));
        assert_eq!(n.pointer("x"), None);
        assert_eq!(n.pointer("/missing"), None);
        assert_eq!(n.clone().into_pointer("/a~1b/~0k/1"), Some(num("20")));
        assert_eq!(n.clone().into_pointer("/a~1b/~0k/+1"), None);
    }

    /// The rendering rule, case by case: (source, cell, verbatim?).
    #[test]
    fn a_number_renders_as_serde_json_did_when_that_was_exact_and_as_written_otherwise() {
        let cases: &[(&str, &str, bool)] = &[
            ("18446744073709551615", "18446744073709551615", false), // u64::MAX
            ("18446744073709551616", "18446744073709551616", true),  // u64::MAX + 1
            ("-9223372036854775808", "-9223372036854775808", false), // i64::MIN
            ("-9223372036854775809", "-9223372036854775809", true),
            ("123456789012345678901", "123456789012345678901", true), // 21 digits
            ("100000000000000000000", "100000000000000000000", true), // a double holds it; `1e20` is not what was written
            ("123456789012345678", "123456789012345678", false),    // exact in i64
            ("0.1", "0.1", false),
            ("0.10", "0.1", false),
            ("1.0", "1.0", false),
            ("1e3", "1000.0", false),
            ("1E400", "1E400", true),  // overflows a double
            ("1e-400", "1e-400", true), // underflows to 0.0
            ("0.123456789012345678901234567891", "0.123456789012345678901234567891", true), // 30 significant
            ("-0", "-0.0", false),
            ("-0.0", "-0.0", false),
            ("0", "0", false),
            ("0.1234567890123456789", "0.1234567890123456789", true),
            ("1.5e300", "1.5e+300", false),
            ("12345678901234567890123", "12345678901234567890123", true),
            ("1e22", "1e+22", false),
            ("2.5E-3", "0.0025", false),
        ];
        for &(src, cell, verbatim) in cases {
            assert_eq!(render_number(src), (cell.to_string(), verbatim), "{src}");
            if !verbatim {
                let serde: serde_json::Number = serde_json::from_str(src).unwrap();
                assert_eq!(cell, serde.to_string(), "{src}: an exact number must render as serde_json renders it");
            }
        }
    }

    /// Exactness is decided on decimal digits, never on floats.
    #[test]
    fn same_decimal_compares_what_the_texts_denote() {
        assert!(same_decimal("1000", "1e3"));
        assert!(same_decimal("1000.0", "1E+3"));
        assert!(same_decimal("0.0025", "2.5e-3"));
        assert!(same_decimal("-0", "0.0"), "a zero is a zero");
        assert!(same_decimal("0.100", "0.1"));
        assert!(!same_decimal("0.1", "0.10000000000000001"));
        assert!(!same_decimal("-1", "1"));
        assert!(!same_decimal("1e99999999999999999999999", "1e22"), "an exponent past i64 saturates");
        assert!(!same_decimal("1e-400", "0.0"));
    }

    #[test]
    fn strings_take_every_escape_and_refuse_a_lone_surrogate() {
        let n = Node::parse(r#""a\"b\\c\/d\b\f\n\r\té€😀""#).unwrap();
        assert_eq!(n, Node::String("a\"b\\c/d\u{8}\u{c}\n\r\t\u{e9}\u{20ac}\u{1f600}".into()));
        for bad in [r#""\ud83d""#, r#""\ud83dx""#, r#""\ude00""#, r#""\ud83dA""#, r#""\x""#, r#""\u12g4""#] {
            let e = Node::parse(bad).unwrap_err();
            assert!(serde_json::from_str::<serde_json::Value>(bad).is_err(), "{bad}: serde_json accepted it");
            assert!(!e.is_eof(), "{bad}: {e}");
        }
        let s = "\u{1}\"\\\n\u{7f}/é";
        let n = Node::String(s.into());
        assert_eq!(n.to_json(), serde_json::to_string(s).unwrap(), "escapes are serde_json's");
    }

    /// Malformed input is refused in serde_json's words and at its position,
    /// and the end of input is told apart from a broken value.
    #[test]
    fn malformed_documents_are_refused_as_serde_json_refuses_them() {
        let cases = [
            "", " ", "{", "[", "[1,", "[1,]", "{\"a\":1,}", "{\"a\" 1}", "{\"a\":1 \"b\":2}", "[1 2]", "{1:2}",
            "tru", "trux", "nul", "-", "-a", "01", "1.", "1.e3", "1e", "1e+", "\"abc", "\"a\u{1}\"", "[1] x",
            "{\"a\":[1,{\"b\":tru", "\"\\u12", "\n\n  {\"a\":\n x}", "{\"a\":1", "{\"a\"", "{\"a\":", "{,}", "[,1]",
            "-01", "+1", ".5", "NaN", "Infinity", "[1,2,]", "{\"a\":1}}",
        ];
        for text in cases {
            let ours = Node::parse(text).expect_err(text);
            let theirs = serde_json::from_str::<serde_json::Value>(text).expect_err(text);
            assert_eq!(ours.to_string(), theirs.to_string(), "{text:?}");
            assert_eq!(ours.is_eof(), theirs.is_eof(), "{text:?}");
        }
    }

    #[test]
    fn nesting_stops_where_serde_json_stops() {
        for depth in [126, 127, 128, 129, 400] {
            let text = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
            let ours = Node::parse(&text);
            let theirs = serde_json::from_str::<serde_json::Value>(&text);
            assert_eq!(ours.is_ok(), theirs.is_ok(), "depth {depth}");
            if let (Err(a), Err(b)) = (ours, theirs) {
                assert_eq!(a.to_string(), b.to_string(), "depth {depth}");
            }
        }
    }
}
