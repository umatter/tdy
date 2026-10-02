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
//! **A number keeps its value, and its written digits wherever a double could
//! not hold them or would print them in exponent form.** [`render`] is the
//! whole rule: a
//! number that serde_json held exactly renders exactly as serde_json renders
//! it (`1.0`, `1e3` → `1000.0`, `-0` → `-0.0`); a number it did not hold
//! exactly renders as the text the file wrote. Only numbers that used to come
//! out wrong change — including 17-digit literals serde_json's two-rounding
//! parse put one ULP off, which a Float64 column now reads correctly rounded.

use std::fmt;

/// One JSON value, objects in document order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Null,
    Bool(bool),
    /// A number, as its cell renders it ([`render`]).
    Number(Num),
    String(String),
    Array(Vec<Node>),
    /// Keys in the order the document wrote them. A key written twice keeps
    /// its first position and its last value, as `serde_json` keeps the last.
    Object(Vec<(String, Node)>),
}

/// A JSON number as the file wrote it. Its cell text is [`render`]'s,
/// worked out only when a cell is asked for: a pass that only wants the keys
/// (NDJSON's header discovery, the sniffer's walks) never renders one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Num {
    src: String,
}

impl Num {
    fn cell(&self) -> String {
        match render(&self.src) {
            Rendered::Same | Rendered::Verbatim => self.src.clone(),
            Rendered::Text(t) => t,
        }
    }

    fn into_cell(self) -> String {
        match render(&self.src) {
            Rendered::Same | Rendered::Verbatim => self.src,
            Rendered::Text(t) => t,
        }
    }
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
    /// number is [`render`]'s text, and an array or object is compact
    /// JSON text ([`Node::to_json`]).
    pub(crate) fn cell(&self) -> String {
        match self {
            Node::Null => String::new(),
            Node::Bool(b) => b.to_string(),
            Node::Number(n) => n.cell(),
            Node::String(s) => s.clone(),
            nested => nested.to_json(),
        }
    }

    /// [`Node::cell`], moving a string or number's text rather than copying it.
    pub(crate) fn into_cell(self) -> String {
        match self {
            Node::Number(n) => n.into_cell(),
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
            Node::Number(n) => out.push_str(&n.cell()),
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

/// [`render`] as (cell text, verbatim?), for the tests that pin the rule.
#[cfg(test)]
fn render_number(src: &str) -> (String, bool) {
    match render(src) {
        Rendered::Same => (src.to_string(), false),
        Rendered::Verbatim => (src.to_string(), true),
        Rendered::Text(t) => (t, false),
    }
}

/// A number's cell.
enum Rendered {
    /// serde_json's rendering, which is the source text itself.
    Same,
    /// The source text, because serde_json's value was not the number, or
    /// because serde_json would print a plain literal in exponent form.
    Verbatim,
    /// serde_json's rendering, which differs from the source (`1e3`).
    Text(String),
}

/// The cell of a number whose source text is `src` (already checked against
/// the RFC 8259 grammar).
///
/// serde_json holds an integer that fits `u64`/`i64` as one, and anything
/// else as the nearest `f64`. When that value is the number the file wrote,
/// the cell is serde_json's rendering of it — what tdy has always produced.
/// When it is not — an integer past
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
fn render(src: &str) -> Rendered {
    let digits = src.strip_prefix('-').unwrap_or(src);
    let integer_syntax = digits.bytes().all(|b| b.is_ascii_digit());
    // At most 18 digits always fits i64/u64, and serde_json prints such an
    // integer as the digits the grammar already made canonical — except -0.
    if integer_syntax && digits.len() <= 18 && src != "-0" {
        return Rendered::Same;
    }
    // A plain literal keeps its written text where serde_json would print the
    // same value in exponent form: `0.00000123` is not `1.23e-6` to a DECIMAL
    // column, which refuses exponent form.
    let plain = !src.bytes().any(|b| b == b'e' || b == b'E');
    let exact = |t: String| {
        if t == src {
            Rendered::Same
        } else if plain && t.contains('e') {
            Rendered::Verbatim
        } else {
            Rendered::Text(t)
        }
    };
    if !integer_syntax {
        if let Some(cell) = shortest_of_short_decimal(src) {
            return exact(cell);
        }
    }
    let Ok(n) = serde_json::from_str::<serde_json::Number>(src) else {
        // Out of a double's range: serde_json refused the whole document.
        return Rendered::Verbatim;
    };
    if n.is_u64() || n.is_i64() {
        return exact(n.to_string());
    }
    if integer_syntax && digits.bytes().any(|b| b != b'0') {
        return Rendered::Verbatim;
    }
    let rendered = n.to_string();
    // The common case — a double's own shortest digits, as a double prints
    // them — needs no decimal arithmetic to know it is exact.
    if rendered == src || same_decimal(src, &rendered) {
        exact(rendered)
    } else {
        Rendered::Verbatim
    }
}

/// [`render`]'s fast path, for the decimals ordinary data is made of:
/// at most 15 significant digits written (trailing zeros included), and a
/// power of ten serde_json applies within ±22. There serde_json's parse is
/// one correctly rounded IEEE operation on exact operands (the digits fit
/// 2^53, the power of ten is exact), and a double holds every decimal of 15
/// digits distinctly, so the shortest rendering of that double is these
/// same digits — exact by construction. What remains is laying them out as
/// serde_json's formatter (zmij) does: fixed notation for a first-digit
/// exponent in -5..=15 (`1000.0`, `12.34`, `0.0025`), `d.ddde±x` otherwise.
/// `None` sends the number down the slow path (zero, too many digits, too
/// large a power). Pinned against the slow path over random decimals.
fn shortest_of_short_decimal(src: &str) -> Option<String> {
    let b = src.as_bytes();
    if !b.iter().any(|&c| matches!(c, b'.' | b'e' | b'E')) {
        return None;
    }
    let negative = b.first() == Some(&b'-');
    let mut i = usize::from(negative);
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int = &b[int_start..i];
    let mut frac: &[u8] = &[];
    if i < b.len() && b[i] == b'.' {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac = &b[start..i];
    }
    let mut exp: i32 = 0;
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        let neg_exp = b.get(i) == Some(&b'-');
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let digits = &b[i..];
        if digits.is_empty() || digits.len() > 4 {
            return None;
        }
        for &d in digits {
            exp = exp * 10 + i32::from(d - b'0');
        }
        if neg_exp {
            exp = -exp;
        }
    }
    let all = || int.iter().chain(frac.iter()).copied();
    let lead = all().take_while(|&d| d == b'0').count();
    let written = int.len() + frac.len() - lead;
    if written == 0 || written > 15 || (exp - frac.len() as i32).abs() > 22 {
        return None;
    }
    let mut digits = [0u8; 15];
    let mut k = 0;
    for d in all().skip(lead) {
        digits[k] = d;
        k += 1;
    }
    while digits[k - 1] == b'0' {
        k -= 1;
    }
    let digits = &digits[..k];
    let text = |d: &[u8]| std::str::from_utf8(d).expect("ASCII digits").to_string();
    // The exponent of the first significant digit, as in 1.234e{de}.
    let de = int.len() as i32 - lead as i32 + exp - 1;
    let mut out = String::with_capacity(k + 8);
    if negative {
        out.push('-');
    }
    if (-5..=15).contains(&de) {
        if k as i32 - 1 <= de {
            out.push_str(&text(digits));
            out.extend(std::iter::repeat_n('0', (de - (k as i32 - 1)) as usize));
            out.push_str(".0");
        } else if de >= 0 {
            let point = de as usize + 1;
            out.push_str(&text(&digits[..point]));
            out.push('.');
            out.push_str(&text(&digits[point..]));
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-de - 1) as usize));
            out.push_str(&text(digits));
        }
    } else {
        out.push(digits[0] as char);
        if k > 1 {
            out.push('.');
            out.push_str(&text(&digits[1..]));
        }
        out.push('e');
        out.push(if de >= 0 { '+' } else { '-' });
        out.push_str(&de.unsigned_abs().to_string());
    }
    Some(out)
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
pub(crate) fn same_decimal(a: &str, b: &str) -> bool {
    match (decimal_of(a), decimal_of(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// An object's keys by hash: the slot of the first key with each hash.
#[derive(Default)]
struct KeySlots {
    hasher: std::collections::hash_map::RandomState,
    first: std::collections::HashMap<u64, usize>,
}

impl KeySlots {
    fn hash(&self, key: &str) -> u64 {
        use std::hash::BuildHasher;
        self.hasher.hash_one(key)
    }

    fn insert(&mut self, key: &str, slot: usize) {
        let h = self.hash(key);
        self.first.entry(h).or_insert(slot);
    }

    /// The slot holding `key`, if any. A hash shared with another key — a
    /// 64-bit collision — is settled by scanning, so it is never wrong.
    fn find(&self, entries: &[(String, Node)], key: &str) -> Option<usize> {
        let &i = self.first.get(&self.hash(key))?;
        if entries[i].0 == key {
            Some(i)
        } else {
            entries.iter().position(|(k, _)| k == key)
        }
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
        Ok(Node::Number(Num { src: self.text[start..self.pos].to_string() }))
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
        // to a few keys a scan is cheapest; past that a map from the key's
        // hash to its slot, as a scan of the entries so far made a
        // 200,000-key object take minutes. The map holds hashes, not copies
        // of the keys (sniffing a million-key object peaked at 278 MB with
        // copies, 204 MB without); a hash that lands on another key falls
        // back to a scan.
        const SCAN: usize = 16;
        let mut entries: Vec<(String, Node)> = Vec::new();
        let mut slot: Option<KeySlots> = None;
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
                Some(slots) => slots.find(&entries, &key),
                None => entries.iter().position(|(k, _)| *k == key),
            };
            match existing {
                Some(i) => entries[i].1 = v,
                None => {
                    if let Some(slots) = &mut slot {
                        slots.insert(&key, entries.len());
                    }
                    entries.push((key, v));
                    if slot.is_none() && entries.len() > SCAN {
                        let mut slots = KeySlots::default();
                        for (i, (k, _)) in entries.iter().enumerate() {
                            slots.insert(k, i);
                        }
                        slot = Some(slots);
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
        Node::Number(Num { src: text.into() })
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

    /// Two keys sharing a hash cannot be told apart by the map; the scan
    /// it falls back to can. Forced here by pointing one key's hash at the
    /// other's slot.
    #[test]
    fn a_hash_collision_between_keys_falls_back_to_a_scan() {
        let entries = vec![("a".to_string(), Node::Null), ("b".to_string(), Node::Bool(true))];
        let mut slots = KeySlots::default();
        slots.insert("a", 0);
        let hb = slots.hash("b");
        slots.first.insert(hb, 0); // "b" now collides with "a"
        assert_eq!(slots.find(&entries, "a"), Some(0));
        assert_eq!(slots.find(&entries, "b"), Some(1));
        assert_eq!(slots.find(&entries[..1], "b"), None);
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
            // A plain literal serde_json would print in exponent form keeps
            // its written text: `1.23e-6` is the same number, and a DECIMAL
            // column refuses it.
            ("0.00000123", "0.00000123", true),
            ("0.000000000000000001", "0.000000000000000001", true),
            ("-0.000001", "-0.000001", true),
            ("10000000000000000.0", "10000000000000000.0", true),
            ("0.00001", "0.00001", false), // serde_json prints 1e-5 as 0.00001
            ("1e-6", "1e-6", false),
        ];
        for &(src, cell, verbatim) in cases {
            assert_eq!(render_number(src), (cell.to_string(), verbatim), "{src}");
            if !verbatim {
                let serde: serde_json::Number = serde_json::from_str(src).unwrap();
                assert_eq!(cell, serde.to_string(), "{src}: an exact number must render as serde_json renders it");
            }
        }
    }

    /// The fast path is the slow path, faster: over random decimals of every
    /// shape the fast path accepts, its cell is serde_json's rendering.
    #[test]
    fn the_fast_path_renders_as_serde_json_does() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut taken = 0;
        for _ in 0..300_000 {
            let digits: String = (0..1 + next(17)).map(|_| char::from(b'0' + next(10) as u8)).collect();
            let digits = digits.trim_start_matches('0');
            let digits = if digits.is_empty() { "0" } else { digits };
            let mut src = String::new();
            if next(2) == 0 {
                src.push('-');
            }
            let point = next(digits.len() as u64 + 1) as usize;
            if point == 0 {
                src.push('0');
            } else {
                src.push_str(&digits[..point]);
            }
            if point < digits.len() {
                src.push('.');
                src.push_str(&digits[point..]);
            }
            match next(4) {
                0 => src.push_str(&format!("e{}", next(40) as i64 - 20)),
                1 => src.push_str(&format!("E+{}", next(30))),
                _ => {}
            }
            if let Some(fast) = shortest_of_short_decimal(&src) {
                taken += 1;
                let n: serde_json::Number = serde_json::from_str(&src).unwrap();
                assert_eq!(fast, n.to_string(), "{src}");
                assert!(same_decimal(&src, &fast), "{src}");
            }
        }
        assert!(taken > 100_000, "{taken}");
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

    /// The proof that the reader changed nothing it was not meant to: every
    /// JSON fixture, read both ways, agrees on structure, strings and every
    /// number serde_json held exactly; a number it did not hold is the
    /// source's text, and nothing else differs. A file one refuses, both do.
    #[test]
    fn every_json_fixture_reads_as_serde_json_read_it_but_for_inexact_numbers() {
        fn same(ours: &Node, theirs: &serde_json::Value, inexact: &mut usize) -> bool {
            use serde_json::Value as V;
            match (ours, theirs) {
                (Node::Null, V::Null) => true,
                (Node::Bool(a), V::Bool(b)) => a == b,
                (Node::String(a), V::String(b)) => a == b,
                (Node::Number(a), V::Number(b)) => {
                    let (cell, verbatim) = render_number(&a.src);
                    if verbatim {
                        *inexact += 1;
                        cell == a.src
                    } else {
                        cell == b.to_string()
                    }
                }
                (Node::Array(a), V::Array(b)) => {
                    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y, inexact))
                }
                (Node::Object(a), V::Object(b)) => {
                    a.len() == b.len()
                        && ours.sorted_entries().into_iter().zip(b).all(|((k, x), (l, y))| k == l && same(x, y, inexact))
                }
                _ => false,
            }
        }
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("json" | "ndjson" | "jsonl")
                ) && !p.to_string_lossy().ends_with(".tdy.json")
                {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata"), &mut files);
        assert!(files.len() >= 20, "{files:?}");
        let (mut docs, mut inexact) = (0usize, 0usize);
        for p in &files {
            let bytes = std::fs::read(p).unwrap();
            let (text, _) = crate::sample::decode_text(&bytes, None);
            // A document, and each line, since an NDJSON file is both a
            // refused document and a set of documents.
            let mut texts: Vec<&str> = vec![&text];
            texts.extend(text.lines().filter(|l| !l.trim().is_empty()));
            for t in texts {
                match (Node::parse(t), serde_json::from_str::<serde_json::Value>(t)) {
                    (Ok(a), Ok(b)) => {
                        docs += 1;
                        assert!(same(&a, &b, &mut inexact), "{}: {t:.200}", p.display());
                    }
                    (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{}", p.display()),
                    (a, b) => panic!("{}: ours {a:?}, serde_json {b:?}", p.display()),
                }
            }
        }
        assert!(docs >= 40, "{docs}");
        // json_shapes_precision.ndjson's amount_lossy, line 1, is the one.
        assert_eq!(inexact, 1);
    }
}
