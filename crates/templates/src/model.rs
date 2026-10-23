//! The template file format (TOML) and its checked, compiled form.
//! The format is documented in docs/TEMPLATES.md.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::expr::{Expr, parse_integer};

/// Arrays and chains never decode more items than this.
pub(crate) const MAX_ITEMS: u64 = 4096;

/// A structure layout: named, typed fields at offsets.
#[derive(Clone, Debug)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Groups templates in lists, e.g. "NTFS".
    pub group: String,
    /// Fixed size in bytes, if the structure has one.
    pub size: Option<u64>,
    /// How many bytes to read when decoding it on its own.
    pub extent: u64,
    /// Shown in template lists (helper templates are hidden).
    pub listed: bool,
    pub signatures: Vec<Signature>,
    /// Text for the structure's one-line summary, with `{field_id}` placeholders.
    pub summary: Option<String>,
    /// Where the template came from: "built-in" or a file path.
    pub source: String,
    pub(crate) fields: Vec<Field>,
}

/// Bytes that must appear at an offset for a template to be suggested.
#[derive(Clone, Debug)]
pub struct Signature {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct Field {
    pub name: String,
    pub id: String,
    pub offset: Expr,
    pub kind: Kind,
    pub description: Option<String>,
    pub hidden: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum Kind {
    Int(IntKind),
    Bytes {
        length: Expr,
    },
    Ascii {
        length: Expr,
    },
    Utf16 {
        length: Expr,
    },
    Guid {
        values: Vec<(String, String)>,
    },
    Uuid,
    Chs,
    FatTime,
    FatDate,
    FatDateTime,
    FileTime,
    UnixTime,
    Struct {
        target: Target,
        count: Option<Count>,
        inline: bool,
    },
    Chain {
        target: Target,
        item_length: Expr,
        until: Option<Expr>,
        max: u64,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct IntKind {
    pub bytes: usize,
    pub signed: bool,
    pub big_endian: bool,
    pub hex: bool,
    pub values: Vec<(i128, String)>,
    pub flags: Vec<(i128, String)>,
}

/// Which template a struct field or chain uses.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    Fixed(String),
    /// Chosen by the value of an expression; no match and no default means
    /// the field is left out.
    Select {
        value: Expr,
        cases: Vec<(i128, String)>,
        default: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum Count {
    Fixed(u64),
    /// As many as fit in the bytes read.
    Fill,
    Expr(Expr),
}

impl Target {
    fn template_ids(&self) -> Vec<&str> {
        match self {
            Target::Fixed(id) => vec![id],
            Target::Select { cases, default, .. } => cases
                .iter()
                .map(|(_, id)| id.as_str())
                .chain(default.as_deref())
                .collect(),
        }
    }
}

/// A short description of a template, for lists.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub group: String,
    pub size: Option<u64>,
    pub extent: u64,
    pub source: String,
}

impl Template {
    pub fn summary(&self) -> TemplateSummary {
        TemplateSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            group: self.group.clone(),
            size: self.size,
            extent: self.extent,
            source: self.source.clone(),
        }
    }

    /// Whether every signature matches at the start of `data`.
    pub fn matches(&self, data: &[u8]) -> bool {
        !self.signatures.is_empty()
            && self.signatures.iter().all(|signature| {
                usize::try_from(signature.offset)
                    .ok()
                    .and_then(|start| data.get(start..start.checked_add(signature.bytes.len())?))
                    .is_some_and(|found| found == signature.bytes.as_slice())
            })
    }

    /// How many signature bytes it checks; more means a more specific match.
    pub fn signature_strength(&self) -> usize {
        self.signatures.iter().map(|s| s.bytes.len()).sum()
    }

    /// Ids of templates this one uses.
    pub(crate) fn references(&self) -> Vec<&str> {
        self.fields
            .iter()
            .flat_map(|field| match &field.kind {
                Kind::Struct { target, .. } | Kind::Chain { target, .. } => target.template_ids(),
                _ => Vec::new(),
            })
            .collect()
    }
}

// ---------------------------------------------------------------- TOML documents

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDoc {
    #[serde(rename = "template", default)]
    templates: Vec<TemplateDoc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateDoc {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    group: String,
    size: Option<u64>,
    extent: Option<u64>,
    #[serde(default = "yes")]
    list: bool,
    #[serde(default)]
    signature: Vec<SignatureDoc>,
    summary: Option<String>,
    #[serde(rename = "field", default)]
    fields: Vec<FieldDoc>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureDoc {
    offset: u64,
    hex: Option<String>,
    ascii: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldDoc {
    name: String,
    id: Option<String>,
    offset: NumberOrExpr,
    #[serde(rename = "type")]
    kind: String,
    length: Option<NumberOrExpr>,
    format: Option<String>,
    #[serde(default)]
    values: BTreeMap<String, String>,
    #[serde(default)]
    flags: BTreeMap<String, String>,
    description: Option<String>,
    #[serde(default)]
    hidden: bool,
    template: Option<String>,
    select: Option<SelectDoc>,
    count: Option<NumberOrExpr>,
    #[serde(default)]
    inline: bool,
    item_length: Option<String>,
    until: Option<String>,
    max: Option<u64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NumberOrExpr {
    Number(u64),
    Text(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectDoc {
    value: String,
    cases: BTreeMap<String, String>,
    default: Option<String>,
}

/// Parses one template file. `source` names it in messages.
pub(crate) fn parse_file(text: &str, source: &str) -> Result<Vec<Template>, String> {
    let doc: FileDoc = toml::from_str(text).map_err(|e| e.to_string())?;
    doc.templates
        .into_iter()
        .map(|t| compile_template(t, source))
        .collect()
}

fn compile_template(doc: TemplateDoc, source: &str) -> Result<Template, String> {
    let context = |message: String| format!("template \"{}\": {message}", doc.id);
    if !is_identifier(&doc.id) {
        return Err(context("ids may only use letters, digits and _".to_owned()));
    }
    let signatures = doc
        .signature
        .iter()
        .map(compile_signature)
        .collect::<Result<Vec<_>, _>>()
        .map_err(context)?;
    let mut fields = Vec::new();
    let mut ids = HashSet::new();
    for field in doc.fields {
        let name = field.name.clone();
        let compiled =
            compile_field(field).map_err(|e| context(format!("field \"{name}\": {e}")))?;
        if !ids.insert(compiled.id.clone()) {
            return Err(context(format!(
                "two fields have the id \"{}\"",
                compiled.id
            )));
        }
        fields.push(compiled);
    }
    let size = doc.size;
    Ok(Template {
        extent: doc.extent.or(size).unwrap_or(4096),
        id: doc.id,
        name: doc.name,
        description: doc.description,
        group: doc.group,
        size,
        listed: doc.list,
        signatures,
        summary: doc.summary,
        source: source.to_owned(),
        fields,
    })
}

fn compile_signature(doc: &SignatureDoc) -> Result<Signature, String> {
    let bytes = match (&doc.hex, &doc.ascii) {
        (Some(hex), None) => parse_hex(hex)?,
        (None, Some(ascii)) => ascii.as_bytes().to_vec(),
        _ => return Err("a signature needs either hex or ascii".to_owned()),
    };
    if bytes.is_empty() {
        return Err("empty signature".to_owned());
    }
    Ok(Signature {
        offset: doc.offset,
        bytes,
    })
}

/// `"55 AA"` or `"55AA"` → `[0x55, 0xAA]`.
pub fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    let digits: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if !digits.len().is_multiple_of(2) || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("\"{text}\" is not a list of hex bytes"));
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn compile_field(doc: FieldDoc) -> Result<Field, String> {
    let id = doc.id.clone().unwrap_or_else(|| id_from_name(&doc.name));
    if !is_identifier(&id) {
        return Err(format!("\"{id}\" is not a valid id"));
    }
    let offset = expr(&doc.offset)?;
    let length = || -> Result<Expr, String> {
        doc.length
            .as_ref()
            .map(expr)
            .transpose()?
            .ok_or_else(|| "needs a length".to_owned())
    };
    let no_lists = |doc: &FieldDoc| {
        if doc.values.is_empty() && doc.flags.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "values and flags only work on integer fields, not {}",
                doc.kind
            ))
        }
    };
    let kind = match doc.kind.as_str() {
        "bytes" => Kind::Bytes { length: length()? },
        "ascii" => Kind::Ascii { length: length()? },
        "utf16" => Kind::Utf16 { length: length()? },
        "guid" => Kind::Guid {
            values: doc
                .values
                .iter()
                .map(|(k, v)| (k.to_ascii_uppercase(), v.clone()))
                .collect(),
        },
        "uuid" => Kind::Uuid,
        "chs" => Kind::Chs,
        "fat_time" => Kind::FatTime,
        "fat_date" => Kind::FatDate,
        "fat_datetime" => Kind::FatDateTime,
        "filetime" => Kind::FileTime,
        "unix_time" => Kind::UnixTime,
        "struct" => Kind::Struct {
            target: target(&doc)?,
            count: doc.count.as_ref().map(count).transpose()?,
            inline: doc.inline,
        },
        "chain" => Kind::Chain {
            target: target(&doc)?,
            item_length: Expr::parse(
                doc.item_length
                    .as_deref()
                    .ok_or("a chain needs item_length")?,
            )?,
            until: doc.until.as_deref().map(Expr::parse).transpose()?,
            max: doc.max.unwrap_or(64).min(MAX_ITEMS),
        },
        other => {
            Kind::Int(int_kind(other, &doc).ok_or_else(|| format!("unknown type \"{other}\""))??)
        }
    };
    if !matches!(kind, Kind::Int(_) | Kind::Guid { .. }) {
        no_lists(&doc)?;
    }
    if doc.inline && !matches!(kind, Kind::Struct { count: None, .. }) {
        return Err("only single structs can be inline".to_owned());
    }
    Ok(Field {
        name: doc.name,
        id,
        offset,
        kind,
        description: doc.description,
        hidden: doc.hidden,
    })
}

/// `u8`…`u64`, `i8`…`i64`, `u24`, `u48`, with a `be` suffix for big-endian.
/// Returns `None` for names that aren't integer types.
fn int_kind(name: &str, doc: &FieldDoc) -> Option<Result<IntKind, String>> {
    let (base, big_endian) = name
        .strip_suffix("be")
        .map_or((name, false), |base| (base, true));
    let signed = base.starts_with('i');
    let bits: usize = base.strip_prefix(['u', 'i'])?.parse().ok()?;
    if !matches!(bits, 8 | 16 | 24 | 32 | 48 | 64) {
        return None;
    }
    let hex = match doc.format.as_deref() {
        None | Some("dec") => false,
        Some("hex") => true,
        Some(other) => return Some(Err(format!("unknown format \"{other}\" (use hex or dec)"))),
    };
    let parse_keys = |map: &BTreeMap<String, String>| -> Result<Vec<(i128, String)>, String> {
        map.iter()
            .map(|(key, name)| {
                parse_integer(key)
                    .map(|value| (value, name.clone()))
                    .ok_or_else(|| format!("\"{key}\" is not a number"))
            })
            .collect()
    };
    Some((|| {
        let mut flags = parse_keys(&doc.flags)?;
        // Show set flags lowest bit first, not in the text order of the keys.
        flags.sort_by_key(|(bit, _)| *bit);
        Ok(IntKind {
            bytes: bits / 8,
            signed,
            big_endian,
            hex,
            values: parse_keys(&doc.values)?,
            flags,
        })
    })())
}

fn target(doc: &FieldDoc) -> Result<Target, String> {
    match (&doc.template, &doc.select) {
        (Some(id), None) => Ok(Target::Fixed(id.clone())),
        (None, Some(select)) => Ok(Target::Select {
            value: Expr::parse(&select.value)?,
            cases: select
                .cases
                .iter()
                .map(|(key, id)| {
                    parse_integer(key)
                        .map(|value| (value, id.clone()))
                        .ok_or_else(|| format!("case \"{key}\" is not a number"))
                })
                .collect::<Result<_, _>>()?,
            default: select.default.clone(),
        }),
        _ => Err("needs either template or select".to_owned()),
    }
}

fn count(doc: &NumberOrExpr) -> Result<Count, String> {
    match doc {
        NumberOrExpr::Number(n) => Ok(Count::Fixed(*n)),
        NumberOrExpr::Text(text) if text == "fill" => Ok(Count::Fill),
        NumberOrExpr::Text(text) => Ok(Count::Expr(Expr::parse(text)?)),
    }
}

fn expr(doc: &NumberOrExpr) -> Result<Expr, String> {
    match doc {
        NumberOrExpr::Number(n) => Ok(Expr::number(i128::from(*n))),
        NumberOrExpr::Text(text) => Expr::parse(text),
    }
}

/// "Bytes per sector" → "bytes_per_sector".
fn id_from_name(name: &str) -> String {
    let mut id = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
        } else if !id.ends_with('_') && !id.is_empty() {
            id.push('_');
        }
    }
    id.trim_end_matches('_').to_owned()
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Checks that every template's references exist and that arrays have a
/// fixed element size. Returns problems; broken templates are removed (and
/// then anything that used them, and so on).
pub(crate) fn check_references(templates: &mut HashMap<String, Template>) -> Vec<String> {
    let mut problems = Vec::new();
    loop {
        let broken: Vec<(String, String)> = templates
            .values()
            .filter_map(|template| {
                problem_with(template, templates).map(|p| (template.id.clone(), p))
            })
            .collect();
        if broken.is_empty() {
            return problems;
        }
        for (id, problem) in broken {
            if let Some(template) = templates.remove(&id) {
                problems.push(format!(
                    "{}: template \"{id}\" was skipped: {problem}",
                    template.source
                ));
            }
        }
    }
}

fn problem_with(template: &Template, all: &HashMap<String, Template>) -> Option<String> {
    for reference in template.references() {
        if !all.contains_key(reference) {
            return Some(format!(
                "it uses template \"{reference}\", which doesn't exist"
            ));
        }
    }
    for field in &template.fields {
        if let Kind::Struct {
            target,
            count: Some(_),
            ..
        } = &field.kind
        {
            match target {
                Target::Fixed(id) if all.get(id).is_some_and(|t| t.size.unwrap_or(0) > 0) => {}
                Target::Fixed(id) => {
                    return Some(format!(
                        "field \"{}\" is an array of \"{id}\", which needs a size",
                        field.name
                    ));
                }
                Target::Select { .. } => {
                    return Some(format!(
                        "field \"{}\" is an array and must use one template (a template that selects is fine)",
                        field.name
                    ));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_names() {
        assert_eq!(id_from_name("Bytes per sector"), "bytes_per_sector");
        assert_eq!(
            id_from_name("First cluster (high 16 bits)"),
            "first_cluster_high_16_bits"
        );
        assert_eq!(
            id_from_name("$LogFile sequence number"),
            "logfile_sequence_number"
        );
    }

    #[test]
    fn hex_signatures() {
        assert_eq!(parse_hex("55 AA").unwrap(), vec![0x55, 0xAA]);
        assert_eq!(parse_hex("eb5290").unwrap(), vec![0xEB, 0x52, 0x90]);
        assert!(parse_hex("5").is_err());
        assert!(parse_hex("zz").is_err());
    }

    fn compile(text: &str) -> Result<Vec<Template>, String> {
        parse_file(text, "test.toml")
    }

    #[test]
    fn reports_template_mistakes() {
        let unknown_type = r#"
            [[template]]
            id = "x"
            name = "X"
            [[template.field]]
            name = "A"
            offset = 0
            type = "u7"
        "#;
        assert!(
            compile(unknown_type)
                .unwrap_err()
                .contains("unknown type \"u7\"")
        );

        let bad_expr = unknown_type.replace("type = \"u7\"", "type = \"bytes\"\nlength = \"a +\"");
        assert!(compile(&bad_expr).unwrap_err().contains("field \"A\""));

        let typo = unknown_type.replace("offset = 0", "ofset = 0");
        assert!(compile(&typo).is_err());

        let duplicate = r#"
            [[template]]
            id = "x"
            name = "X"
            [[template.field]]
            name = "A"
            offset = 0
            type = "u8"
            [[template.field]]
            name = "A"
            offset = 1
            type = "u8"
        "#;
        assert!(compile(duplicate).unwrap_err().contains("two fields"));
    }

    #[test]
    fn broken_references_remove_templates_and_their_users() {
        let text = r#"
            [[template]]
            id = "outer"
            name = "Outer"
            [[template.field]]
            name = "Inner"
            offset = 0
            type = "struct"
            template = "middle"

            [[template]]
            id = "middle"
            name = "Middle"
            [[template.field]]
            name = "Missing"
            offset = 0
            type = "struct"
            template = "nowhere"

            [[template]]
            id = "fine"
            name = "Fine"
            [[template.field]]
            name = "A"
            offset = 0
            type = "u8"
        "#;
        let mut templates: HashMap<String, Template> = compile(text)
            .unwrap()
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        let problems = check_references(&mut templates);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(templates.contains_key("fine"));
        assert_eq!(templates.len(), 1);
    }
}
