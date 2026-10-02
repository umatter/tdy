//! A JSON document that remembers the order its keys were written in.
//!
//! `serde_json::Value` keeps an object's keys sorted (tdy builds it without
//! `preserve_order`, and turning that feature on would reorder every map in
//! the dependency tree that shares the crate). For a record array that is
//! harmless — the header is a union of many records' keys anyway — but a
//! document read as ONE record (`record = true`) has exactly one key order,
//! the author's, and a header that alphabetises `{id, name, category}` into
//! `{category, id, name}` reads as somebody else's table. So the one object
//! that is a record, and the leaves `tdy draft` walks, are parsed through
//! this instead.
//!
//! Scalars are kept as `serde_json::Value`, deserialised through the same
//! visitor calls `Value` itself uses, so a number renders identically
//! whichever of the two read it.

use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// One JSON value, objects in document order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    /// null, a boolean, a number or a string.
    Scalar(serde_json::Value),
    Array(Vec<Node>),
    /// Keys in the order the document wrote them. A key written twice keeps
    /// its first position and its last value, as `serde_json` keeps the last.
    Object(Vec<(String, Node)>),
}

impl Node {
    pub(crate) fn parse(text: &str) -> serde_json::Result<Node> {
        serde_json::from_str(text)
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
            let token = raw.replace("~1", "/").replace("~0", "~");
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
                Node::Scalar(_) => return None,
            };
        }
        Some(at)
    }

    /// The same value as `serde_json` holds it (keys sorted), for rendering a
    /// nested value as JSON text exactly as a record array's cell renders it.
    pub(crate) fn to_value(&self) -> serde_json::Value {
        match self {
            Node::Scalar(v) => v.clone(),
            Node::Array(items) => serde_json::Value::Array(items.iter().map(Node::to_value).collect()),
            Node::Object(entries) => {
                serde_json::Value::Object(entries.iter().map(|(k, v)| (k.clone(), v.to_value())).collect())
            }
        }
    }

    /// "an object", "an array", "a number", … — for messages.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Node::Object(_) => "an object",
            Node::Array(_) => "an array",
            Node::Scalar(serde_json::Value::Null) => "null",
            Node::Scalar(serde_json::Value::Bool(_)) => "a boolean",
            Node::Scalar(serde_json::Value::Number(_)) => "a number",
            Node::Scalar(_) => "a string",
        }
    }
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Node, D::Error> {
        d.deserialize_any(NodeVisitor)
    }
}

struct NodeVisitor;

impl<'de> Visitor<'de> for NodeVisitor {
    type Value = Node;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Node, E> {
        Ok(Node::Scalar(v.into()))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Node, E> {
        Ok(Node::Scalar(v.into()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Node, E> {
        Ok(Node::Scalar(v.into()))
    }
    fn visit_f64<E>(self, v: f64) -> Result<Node, E> {
        Ok(Node::Scalar(serde_json::Number::from_f64(v).map_or(serde_json::Value::Null, serde_json::Value::Number)))
    }
    fn visit_str<E>(self, v: &str) -> Result<Node, E> {
        Ok(Node::Scalar(v.into()))
    }
    fn visit_string<E>(self, v: String) -> Result<Node, E> {
        Ok(Node::Scalar(v.into()))
    }
    fn visit_unit<E>(self) -> Result<Node, E> {
        Ok(Node::Scalar(serde_json::Value::Null))
    }
    fn visit_none<E>(self) -> Result<Node, E> {
        Ok(Node::Scalar(serde_json::Value::Null))
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Node, D::Error> {
        Node::deserialize(d)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
        let mut items = Vec::new();
        while let Some(v) = seq.next_element()? {
            items.push(v);
        }
        Ok(Node::Array(items))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
        let mut entries: Vec<(String, Node)> = Vec::new();
        while let Some((k, v)) = map.next_entry::<String, Node>()? {
            match entries.iter_mut().find(|(have, _)| *have == k) {
                Some(slot) => slot.1 = v,
                None => entries.push((k, v)),
            }
        }
        Ok(Node::Object(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_keep_the_documents_order_and_values_render_as_serde_json_does() {
        let n = Node::parse(r#"{"z":1,"a":{"y":2.5,"b":[true,null]},"m":"x","z":3}"#).unwrap();
        let Node::Object(e) = &n else { panic!() };
        let keys: Vec<&str> = e.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["z", "a", "m"]);
        assert_eq!(n.pointer("/z"), Some(&Node::Scalar(3.into())), "the last value wins");
        let text = r#"{"z":1,"a":{"y":2.5,"b":[true,null]},"m":"x"}"#;
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(Node::parse(text).unwrap().to_value(), v);
    }

    #[test]
    fn pointers_read_as_rfc_6901_says() {
        let n = Node::parse(r#"{"a/b":{"~k":[10,20]},"":1}"#).unwrap();
        assert_eq!(n.pointer("/a~1b/~0k/1"), Some(&Node::Scalar(20.into())));
        assert_eq!(n.pointer("/a~1b/~0k/01"), None);
        assert_eq!(n.pointer("/"), Some(&Node::Scalar(1.into())));
        assert_eq!(n.pointer(""), Some(&n));
        assert_eq!(n.pointer("x"), None);
        assert_eq!(n.pointer("/missing"), None);
    }
}
