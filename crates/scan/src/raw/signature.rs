//! File signatures: what raw recovery looks for, read from a TOML list
//! (the built-in one is `signatures.toml`, next to this file).

use std::path::Path;

use serde::Deserialize;

/// The built-in list, in the same format users edit.
pub const DEFAULT_SIGNATURES: &str = include_str!("signatures.toml");

/// Files are cut here when nothing says where they end.
const DEFAULT_MAX_SIZE: u64 = 64 << 20;

/// What raw recovery looks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub extension: String,
    /// The bytes a file starts with; `None` matches any byte.
    pub header: Vec<Option<u8>>,
    /// Where the header is in the file.
    pub header_offset: usize,
    pub end: End,
    pub max_size: u64,
    pub min_size: u64,
}

/// How to tell where a file ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// A format DataReco reads and checks.
    Parser(Parser),
    /// The file ends after these bytes.
    Footer(Vec<u8>),
    /// The header stores the file's size.
    Field {
        offset: usize,
        bytes: usize,
        big_endian: bool,
        add: u64,
    },
    /// Nothing says: cut at the next file found, or at the maximum size.
    Unknown,
}

/// The formats DataReco reads to find a file's end (and check it's real).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parser {
    Jpeg,
    Png,
    Gif,
    Bmp,
    Tiff,
    Riff,
    Zip,
    Pdf,
    IsoBmff,
    Mp3,
    Ogg,
    Ebml,
    SevenZip,
    Rar,
    Sqlite,
    Ole,
    Pe,
}

impl Parser {
    const ALL: [(&'static str, Parser); 17] = [
        ("jpeg", Parser::Jpeg),
        ("png", Parser::Png),
        ("gif", Parser::Gif),
        ("bmp", Parser::Bmp),
        ("tiff", Parser::Tiff),
        ("riff", Parser::Riff),
        ("zip", Parser::Zip),
        ("pdf", Parser::Pdf),
        ("isobmff", Parser::IsoBmff),
        ("mp3", Parser::Mp3),
        ("ogg", Parser::Ogg),
        ("ebml", Parser::Ebml),
        ("7z", Parser::SevenZip),
        ("rar", Parser::Rar),
        ("sqlite", Parser::Sqlite),
        ("ole", Parser::Ole),
        ("pe", Parser::Pe),
    ];

    fn named(name: &str) -> Option<Parser> {
        Self::ALL
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|&(_, p)| p)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDoc {
    #[serde(default)]
    signature: Vec<SignatureDoc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureDoc {
    name: String,
    extension: String,
    header: String,
    #[serde(default)]
    header_offset: usize,
    size: Option<String>,
    footer: Option<String>,
    size_field: Option<FieldDoc>,
    max_size: Option<toml::Value>,
    min_size: Option<toml::Value>,
    #[serde(default = "enabled")]
    enabled: bool,
}

fn enabled() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldDoc {
    offset: usize,
    bytes: usize,
    #[serde(default = "little")]
    endian: String,
    #[serde(default)]
    add: u64,
}

fn little() -> String {
    "little".to_owned()
}

/// The built-in signatures.
pub fn default_signatures() -> Vec<Signature> {
    parse_signatures(DEFAULT_SIGNATURES).expect("the built-in signatures are valid")
}

/// Reads a signature list; the error names the signature that's wrong.
pub fn parse_signatures(text: &str) -> Result<Vec<Signature>, String> {
    let doc: FileDoc = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut signatures = Vec::new();
    for (i, item) in doc.signature.into_iter().enumerate() {
        let name = item.name.clone();
        let wrong = |what: String| format!("signature {} (\"{name}\"): {what}", i + 1);
        if !item.enabled {
            continue;
        }
        let header = parse_pattern(&item.header).map_err(wrong)?;
        if header.is_empty() || header.iter().all(Option::is_none) {
            return Err(wrong(
                "the header needs at least one byte that isn't ??".to_owned(),
            ));
        }
        let ways = [
            item.size.is_some(),
            item.footer.is_some(),
            item.size_field.is_some(),
        ];
        if ways.iter().filter(|&&w| w).count() > 1 {
            return Err(wrong(
                "give only one of size, footer and size_field".to_owned(),
            ));
        }
        let end = if let Some(parser) = &item.size {
            End::Parser(Parser::named(parser).ok_or_else(|| {
                let known: Vec<&str> = Parser::ALL.iter().map(|(n, _)| *n).collect();
                wrong(format!(
                    "size \"{parser}\" isn't one of {}",
                    known.join(", ")
                ))
            })?)
        } else if let Some(footer) = &item.footer {
            let bytes = parse_pattern(footer).map_err(wrong)?;
            if bytes.is_empty() || bytes.iter().any(Option::is_none) {
                return Err(wrong(
                    "the footer must be whole bytes, without ??".to_owned(),
                ));
            }
            End::Footer(bytes.into_iter().flatten().collect())
        } else if let Some(field) = &item.size_field {
            if !(1..=8).contains(&field.bytes) {
                return Err(wrong("size_field.bytes must be 1 to 8".to_owned()));
            }
            let big_endian = match field.endian.to_ascii_lowercase().as_str() {
                "little" => false,
                "big" => true,
                other => return Err(wrong(format!("endian \"{other}\" isn't little or big"))),
            };
            End::Field {
                offset: field.offset,
                bytes: field.bytes,
                big_endian,
                add: field.add,
            }
        } else {
            End::Unknown
        };
        let max_size = match &item.max_size {
            Some(value) => parse_size(value).map_err(wrong)?,
            None => DEFAULT_MAX_SIZE,
        };
        let min_size = match &item.min_size {
            Some(value) => parse_size(value).map_err(wrong)?,
            None => 0,
        };
        let extension = item.extension.trim_start_matches('.').to_owned();
        if extension.is_empty() || extension.contains(['/', '\\', ':']) {
            return Err(wrong(format!(
                "\"{}\" isn't a file extension",
                item.extension
            )));
        }
        signatures.push(Signature {
            name: item.name,
            extension,
            header,
            header_offset: item.header_offset,
            end,
            max_size,
            min_size,
        });
    }
    Ok(signatures)
}

/// Reads the signature list at `path`.
pub fn load_signatures(path: &Path) -> Result<Vec<Signature>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("couldn't read {}: {e}", path.display()))?;
    parse_signatures(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// "FF D8 ??" → bytes, `None` for ??.
fn parse_pattern(text: &str) -> Result<Vec<Option<u8>>, String> {
    let compact: String = text.split_whitespace().collect();
    if !compact.len().is_multiple_of(2) {
        return Err(format!("\"{text}\" isn't whole hex bytes"));
    }
    compact
        .as_bytes()
        .chunks(2)
        .map(|pair| match pair {
            b"??" => Ok(None),
            _ => std::str::from_utf8(pair)
                .ok()
                .and_then(|p| u8::from_str_radix(p, 16).ok())
                .map(Some)
                .ok_or_else(|| format!("\"{text}\" isn't hex bytes (or ??)")),
        })
        .collect()
}

/// `1048576`, `"512 KiB"`, `"200 MiB"`, `"4 GiB"`.
fn parse_size(value: &toml::Value) -> Result<u64, String> {
    match value {
        toml::Value::Integer(n) => u64::try_from(*n).map_err(|_| format!("{n} isn't a size")),
        toml::Value::String(text) => {
            let text = text.trim();
            let split = text
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(text.len());
            let (number, unit) = text.split_at(split);
            let number: u64 = number
                .parse()
                .map_err(|_| format!("\"{text}\" isn't a size"))?;
            let factor: u64 = match unit.trim().to_ascii_lowercase().as_str() {
                "" | "b" | "bytes" => 1,
                "kib" | "kb" | "k" => 1 << 10,
                "mib" | "mb" | "m" => 1 << 20,
                "gib" | "gb" | "g" => 1 << 30,
                "tib" | "tb" | "t" => 1 << 40,
                _ => {
                    return Err(format!(
                        "\"{text}\" isn't a size (use bytes, KiB, MiB or GiB)"
                    ));
                }
            };
            number
                .checked_mul(factor)
                .ok_or_else(|| format!("\"{text}\" is too big"))
        }
        other => Err(format!("{other} isn't a size")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_list_is_valid() {
        let signatures = default_signatures();
        assert!(signatures.len() >= 18);
        let jpeg = &signatures[0];
        assert_eq!(
            (jpeg.name.as_str(), jpeg.extension.as_str()),
            ("JPEG image", "jpg")
        );
        assert_eq!(jpeg.header, [Some(0xFF), Some(0xD8), Some(0xFF)]);
        assert_eq!(jpeg.end, End::Parser(Parser::Jpeg));
        assert_eq!(jpeg.max_size, 200 << 20);
        // The HTML example is there but off.
        assert!(signatures.iter().all(|s| s.extension != "html"));
    }

    #[test]
    fn custom_signatures_and_mistakes() {
        let text = r#"
            [[signature]]
            name = "Mine"
            extension = ".myf"
            header = "4D 59 ?? 01"
            header_offset = 2
            size_field = { offset = 8, bytes = 4, endian = "big", add = 16 }
            max_size = "1 MiB"
            min_size = 100

            [[signature]]
            name = "Ends with END!"
            extension = "end"
            header = "53 54 41 52 54"
            footer = "45 4E 44 21"
        "#;
        let list = parse_signatures(text).unwrap();
        assert_eq!(list[0].extension, "myf");
        assert_eq!(list[0].header, [Some(0x4D), Some(0x59), None, Some(1)]);
        assert_eq!(
            list[0].end,
            End::Field {
                offset: 8,
                bytes: 4,
                big_endian: true,
                add: 16
            }
        );
        assert_eq!((list[0].max_size, list[0].min_size), (1 << 20, 100));
        assert_eq!(list[1].end, End::Footer(b"END!".to_vec()));
        assert_eq!(list[1].max_size, 64 << 20);

        let bad = |body: &str| {
            parse_signatures(&format!(
                "[[signature]]\nname = \"X\"\nextension = \"x\"\n{body}"
            ))
            .unwrap_err()
        };
        assert!(bad("header = \"GG\"").contains("signature 1 (\"X\")"));
        assert!(bad("header = \"?? ??\"").contains("at least one byte"));
        assert!(bad("header = \"01\"\nsize = \"jpg\"").contains("isn't one of"));
        assert!(bad("header = \"01\"\nsize = \"png\"\nfooter = \"02\"").contains("only one"));
        assert!(bad("header = \"01\"\nmax_size = \"3 parsecs\"").contains("isn't a size"));
        assert!(bad("header = \"01\"\ncolour = \"red\"").contains("colour"));
    }
}
