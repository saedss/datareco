//! Applying a template to bytes. Never panics on any input: problems become
//! fields marked `problem` in the result.

use std::collections::HashMap;

use serde::Serialize;

use crate::expr::{Expr, Scope};
use crate::format::{
    ascii_text, bytes_text, chs_text, fat_date_text, fat_datetime_text, fat_time_text,
    filetime_text, guid_text, int_note, int_text, read_int, unix_text, utf16_text,
};
use crate::model::{Count, Field, Kind, MAX_ITEMS, Target, Template};

/// Structures nest at most this deep.
const MAX_DEPTH: u32 = 16;
/// A decode produces at most this many fields.
const MAX_NODES: usize = 20_000;

/// One decoded field or structure.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub name: String,
    /// Absolute byte offset (from the start of the disk or image).
    pub offset: u64,
    pub size: u64,
    /// The value as text; for structures, a one-line summary.
    pub value: String,
    /// What the value means (a named value or set flags), or what went wrong.
    pub note: Option<String>,
    pub description: Option<String>,
    /// The field couldn't be decoded (past the end of the data, bad offset, …).
    pub problem: bool,
    pub children: Vec<Node>,
}

impl Node {
    fn problem(name: &str, offset: u64, message: String) -> Node {
        Node {
            name: name.to_owned(),
            offset,
            size: 0,
            value: String::new(),
            note: Some(message),
            description: None,
            problem: true,
            children: Vec::new(),
        }
    }

    /// The first descendant with this name, depth first.
    pub fn find(&self, name: &str) -> Option<&Node> {
        self.children.iter().find_map(|child| {
            if child.name == name {
                Some(child)
            } else {
                child.find(name)
            }
        })
    }
}

pub(crate) struct Decoder<'a> {
    pub templates: &'a HashMap<String, Template>,
    pub data: &'a [u8],
    pub base: u64,
    pub nodes_left: usize,
}

/// The result of decoding one template at one place.
struct Decoded {
    nodes: Vec<Node>,
    size: u64,
    scope: Scope,
    summary: Option<String>,
}

impl<'a> Decoder<'a> {
    pub fn new(templates: &'a HashMap<String, Template>, data: &'a [u8], base: u64) -> Self {
        Self {
            templates,
            data,
            base,
            nodes_left: MAX_NODES,
        }
    }

    /// Decodes `template` at the start of the data.
    pub fn decode_root(&mut self, template: &Template) -> Node {
        let decoded = self.decode(template, 0, 0);
        Node {
            name: template.name.clone(),
            offset: self.base,
            size: decoded.size,
            value: decoded.summary.unwrap_or_default(),
            note: None,
            description: non_empty(&template.description),
            problem: false,
            children: decoded.nodes,
        }
    }

    fn decode(&mut self, template: &Template, start: u64, depth: u32) -> Decoded {
        let mut nodes = Vec::new();
        let mut scope = Scope::new();
        let mut texts: HashMap<String, String> = HashMap::new();
        let mut inline_summary = None;
        let mut end = start;

        if depth > MAX_DEPTH {
            nodes.push(Node::problem(
                &template.name,
                self.abs(start),
                "nested too deeply".to_owned(),
            ));
        } else {
            for field in &template.fields {
                if self.nodes_left == 0 {
                    nodes.push(Node::problem(
                        &field.name,
                        self.abs(start),
                        "too many fields to show".to_owned(),
                    ));
                    break;
                }
                self.nodes_left -= 1;
                let Some(result) = self.field(field, start, depth, &scope) else {
                    continue;
                };
                end = end.max(
                    result
                        .node
                        .offset
                        .saturating_sub(self.base)
                        .saturating_add(result.node.size),
                );
                if let Some(value) = result.value {
                    scope.insert(field.id.clone(), value);
                }
                if let Some(text) = result.text {
                    texts.insert(field.id.clone(), text);
                }
                if result.inline {
                    if inline_summary.is_none() {
                        inline_summary = Some(result.node.value.clone());
                    }
                    nodes.extend(result.node.children);
                } else if !field.hidden {
                    nodes.push(result.node);
                }
            }
        }

        let summary = template
            .summary
            .as_ref()
            .map(|format| fill_summary(format, &texts))
            .or(inline_summary.filter(|s| !s.is_empty()));
        Decoded {
            nodes,
            size: template.size.unwrap_or(end - start),
            scope,
            summary,
        }
    }

    /// Decodes one field. `None` means the field is left out (a select
    /// with no matching case).
    fn field(
        &mut self,
        field: &Field,
        start: u64,
        depth: u32,
        scope: &Scope,
    ) -> Option<FieldResult> {
        let offset = match eval_offset(&field.offset, scope).and_then(|rel| {
            start
                .checked_add(rel)
                .ok_or_else(|| "offset too large".to_owned())
        }) {
            Ok(offset) => offset,
            Err(e) => {
                return Some(FieldResult::node(Node::problem(
                    &field.name,
                    self.abs(start),
                    format!("offset: {e}"),
                )));
            }
        };
        let at = self.abs(offset);
        let mut node = Node {
            name: field.name.clone(),
            offset: at,
            size: 0,
            value: String::new(),
            note: None,
            description: field.description.clone(),
            problem: false,
            children: Vec::new(),
        };
        let mut result = FieldResult {
            node: Node::problem("", 0, String::new()),
            value: None,
            text: None,
            inline: false,
        };

        macro_rules! read {
            ($len:expr) => {
                match self.bytes(offset, $len) {
                    Some(bytes) => bytes,
                    None => {
                        node.size = $len;
                        return Some(FieldResult::node(past_end(node)));
                    }
                }
            };
        }
        macro_rules! length {
            ($expr:expr) => {
                match eval_offset($expr, scope) {
                    Ok(len) => len,
                    Err(e) => {
                        return Some(FieldResult::node(Node::problem(
                            &field.name,
                            at,
                            format!("length: {e}"),
                        )))
                    }
                }
            };
        }

        match &field.kind {
            Kind::Int(kind) => {
                let bytes = read!(kind.bytes as u64);
                let value = read_int(bytes, kind);
                node.size = kind.bytes as u64;
                node.value = int_text(value, kind);
                node.note = int_note(value, kind);
                result.value = Some(value);
                result.text = Some(node.note.clone().unwrap_or_else(|| node.value.clone()));
            }
            Kind::Bytes { length } => {
                let len = length!(length);
                node.size = len;
                node.value = bytes_text(read!(len));
            }
            Kind::Ascii { length } => {
                let len = length!(length);
                let text = ascii_text(read!(len));
                node.size = len;
                node.value = format!("\"{text}\"");
                result.text = Some(text.trim_end().to_owned());
            }
            Kind::Utf16 { length } => {
                let len = length!(length);
                let text = utf16_text(read!(len));
                node.size = len;
                node.value = format!("\"{text}\"");
                result.text = Some(text);
            }
            Kind::Guid { values } => {
                let bytes: [u8; 16] = read!(16).try_into().expect("read 16 bytes");
                node.size = 16;
                node.value = guid_text(&bytes);
                node.note = values
                    .iter()
                    .find(|(guid, _)| *guid == node.value)
                    .map(|(_, name)| name.clone());
                result.text = Some(node.note.clone().unwrap_or_else(|| node.value.clone()));
            }
            Kind::Uuid => {
                let bytes: [u8; 16] = read!(16).try_into().expect("read 16 bytes");
                node.size = 16;
                node.value = crate::format::uuid_text(&bytes);
            }
            Kind::Chs => {
                let bytes: [u8; 3] = read!(3).try_into().expect("read 3 bytes");
                node.size = 3;
                node.value = chs_text(&bytes);
            }
            Kind::FatTime | Kind::FatDate => {
                let bytes = read!(2);
                let value = u16::from_le_bytes([bytes[0], bytes[1]]);
                node.size = 2;
                node.value = if matches!(field.kind, Kind::FatTime) {
                    fat_time_text(value)
                } else {
                    fat_date_text(value)
                };
            }
            Kind::FatDateTime | Kind::UnixTime => {
                let bytes = read!(4);
                let value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                node.size = 4;
                node.value = if matches!(field.kind, Kind::FatDateTime) {
                    fat_datetime_text(value)
                } else if value == 0 {
                    "not set".to_owned()
                } else {
                    unix_text(i64::from(value))
                };
            }
            Kind::FileTime => {
                let bytes = read!(8);
                node.size = 8;
                node.value =
                    filetime_text(u64::from_le_bytes(bytes.try_into().expect("read 8 bytes")));
            }
            Kind::Struct {
                target,
                count,
                inline,
            } => {
                let template = match self.resolve(target, scope) {
                    Ok(Some(template)) => template,
                    Ok(None) => return None,
                    Err(e) => return Some(FieldResult::node(Node::problem(&field.name, at, e))),
                };
                match count {
                    None => {
                        let decoded = self.decode(template, offset, depth + 1);
                        node.size = decoded.size;
                        node.value = decoded.summary.unwrap_or_else(|| template.name.clone());
                        node.children = decoded.nodes;
                        result.inline = *inline;
                    }
                    Some(count) => {
                        let stride = template.size.unwrap_or(0).max(1);
                        let wanted = match count {
                            Count::Fixed(n) => Ok(*n),
                            Count::Fill => {
                                Ok((self.data.len() as u64).saturating_sub(offset) / stride)
                            }
                            Count::Expr(expr) => eval_offset(expr, scope),
                        };
                        let wanted = match wanted {
                            Ok(n) => n,
                            Err(e) => {
                                return Some(FieldResult::node(Node::problem(
                                    &field.name,
                                    at,
                                    format!("count: {e}"),
                                )));
                            }
                        };
                        let n = wanted.min(MAX_ITEMS);
                        for i in 0..n {
                            if self.nodes_left == 0 {
                                break;
                            }
                            self.nodes_left -= 1;
                            let item_offset = offset + i * stride;
                            let decoded = self.decode(template, item_offset, depth + 1);
                            node.children.push(Node {
                                name: format!("[{i}]"),
                                offset: self.abs(item_offset),
                                size: stride,
                                value: decoded.summary.unwrap_or_default(),
                                note: None,
                                description: None,
                                problem: false,
                                children: decoded.nodes,
                            });
                        }
                        node.size = n * stride;
                        node.value = format!("{n} × {}", template.name);
                        if wanted > n {
                            node.note = Some(format!("only the first {n} of {wanted} are shown"));
                        }
                    }
                }
            }
            Kind::Chain {
                target,
                item_length,
                until,
                max,
            } => {
                let mut pos = offset;
                for i in 0..*max {
                    if pos >= self.data.len() as u64 || self.nodes_left == 0 {
                        break;
                    }
                    self.nodes_left -= 1;
                    let template = match self.resolve(target, scope) {
                        Ok(Some(template)) => template,
                        Ok(None) => break,
                        Err(e) => {
                            node.children
                                .push(Node::problem(&format!("[{i}]"), self.abs(pos), e));
                            break;
                        }
                    };
                    let decoded = self.decode(template, pos, depth + 1);
                    if let Some(until) = until
                        && until.eval(&decoded.scope).is_ok_and(|v| v != 0)
                    {
                        break;
                    }
                    let len = match eval_offset(item_length, &decoded.scope) {
                        Ok(len) if len > 0 => len,
                        Ok(_) => {
                            node.children.push(Node::problem(
                                &format!("[{i}]"),
                                self.abs(pos),
                                "item length is 0".to_owned(),
                            ));
                            break;
                        }
                        Err(e) => {
                            node.children.push(Node::problem(
                                &format!("[{i}]"),
                                self.abs(pos),
                                format!("item length: {e}"),
                            ));
                            break;
                        }
                    };
                    node.children.push(Node {
                        name: format!("[{i}]"),
                        offset: self.abs(pos),
                        size: len,
                        value: decoded.summary.unwrap_or_else(|| template.name.clone()),
                        note: None,
                        description: None,
                        problem: false,
                        children: decoded.nodes,
                    });
                    pos = pos.saturating_add(len);
                }
                node.size = pos - offset;
                node.value = format!("{} items", node.children.len());
            }
        }
        result.node = node;
        Some(result)
    }

    fn resolve(&self, target: &Target, scope: &Scope) -> Result<Option<&'a Template>, String> {
        let id = match target {
            Target::Fixed(id) => Some(id),
            Target::Select {
                value,
                cases,
                default,
            } => {
                let value = value.eval(scope).map_err(|e| format!("select: {e}"))?;
                cases
                    .iter()
                    .find(|(case, _)| *case == value)
                    .map(|(_, id)| id)
                    .or(default.as_ref())
            }
        };
        match id {
            None => Ok(None),
            Some(id) => self
                .templates
                .get(id)
                .map(Some)
                .ok_or_else(|| format!("no template named \"{id}\"")),
        }
    }

    fn bytes(&self, offset: u64, len: u64) -> Option<&'a [u8]> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        self.data.get(start..end)
    }

    fn abs(&self, offset: u64) -> u64 {
        self.base.saturating_add(offset)
    }
}

struct FieldResult {
    node: Node,
    /// The integer value, for later fields' expressions.
    value: Option<i128>,
    /// Plain text for summaries.
    text: Option<String>,
    /// Show the struct's fields in place of the struct.
    inline: bool,
}

impl FieldResult {
    fn node(node: Node) -> FieldResult {
        FieldResult {
            node,
            value: None,
            text: None,
            inline: false,
        }
    }
}

fn past_end(mut node: Node) -> Node {
    node.problem = true;
    node.note = Some("past the end of the bytes read".to_owned());
    node
}

/// Evaluates an offset, length or count: must be zero or positive.
fn eval_offset(expr: &Expr, scope: &Scope) -> Result<u64, String> {
    let value = expr.eval(scope)?;
    u64::try_from(value).map_err(|_| format!("{value} is negative or too large"))
}

/// Replaces `{id}` with field text; tidies the dots and spaces left by empty
/// parts (FAT names without an extension).
fn fill_summary(format: &str, texts: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let id = &rest[open + 1..open + close];
        out.push_str(texts.get(id).map_or("", String::as_str));
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out.trim().trim_end_matches('.').trim_end().to_owned()
}

fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries() {
        let texts: HashMap<String, String> = [
            ("name".to_owned(), "README".to_owned()),
            ("extension".to_owned(), "TXT".to_owned()),
        ]
        .into();
        assert_eq!(fill_summary("{name}.{extension}", &texts), "README.TXT");
        let no_extension: HashMap<String, String> = [("name".to_owned(), "DOCS".to_owned())].into();
        assert_eq!(fill_summary("{name}.{extension}", &no_extension), "DOCS");
        assert_eq!(
            fill_summary(
                "Stream: {size} bytes",
                &[("size".to_owned(), "300".to_owned())].into()
            ),
            "Stream: 300 bytes"
        );
        assert_eq!(fill_summary("broken {name", &texts), "broken {name");
    }
}
