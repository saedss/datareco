//! Structure templates for the disk editor: decode raw bytes into named
//! fields (MBR, GPT, boot sectors, MFT records, directory entries,
//! superblocks).
//!
//! Templates are TOML files (format: docs/TEMPLATES.md). The built-in ones
//! live in `src/builtin/`; users add their own in a folder. Decoding never
//! panics on any bytes: problems show up as fields marked `problem`.

mod decode;
mod expr;
mod format;
mod model;

use std::collections::HashMap;
use std::path::Path;

pub use decode::Node;
pub use expr::{Expr, parse_integer};
pub use model::{Signature, Template, TemplateSummary, parse_hex};

/// The built-in template files, embedded in the program.
const BUILTIN: [(&str, &str); 6] = [
    ("mbr.toml", include_str!("builtin/mbr.toml")),
    ("gpt.toml", include_str!("builtin/gpt.toml")),
    ("fat.toml", include_str!("builtin/fat.toml")),
    ("exfat.toml", include_str!("builtin/exfat.toml")),
    ("ntfs.toml", include_str!("builtin/ntfs.toml")),
    ("ext4.toml", include_str!("builtin/ext4.toml")),
];

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("there is no template named \"{0}\"")]
    Unknown(String),
}

/// All loaded templates, by id.
#[derive(Clone, Debug, Default)]
pub struct TemplateSet {
    templates: HashMap<String, Template>,
    problems: Vec<String>,
}

impl TemplateSet {
    /// The templates that ship with DataReco.
    pub fn builtin() -> TemplateSet {
        let mut set = TemplateSet::default();
        for (name, text) in BUILTIN {
            set.add_file(text, &format!("built-in {name}"));
        }
        set.check();
        set
    }

    /// Adds the templates in one TOML file; later files override templates
    /// with the same id. Call [`TemplateSet::check`] after adding files.
    /// Returns how many templates were added; problems are recorded.
    pub fn add_file(&mut self, text: &str, source: &str) -> usize {
        match model::parse_file(text, source) {
            Ok(templates) => {
                let count = templates.len();
                for template in templates {
                    self.templates.insert(template.id.clone(), template);
                }
                count
            }
            Err(e) => {
                self.problems.push(format!("{source}: {e}"));
                0
            }
        }
    }

    /// Adds every `*.toml` file in `dir`, in name order. A missing folder is
    /// fine. Returns how many templates were added.
    pub fn add_dir(&mut self, dir: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut paths: Vec<_> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
            })
            .collect();
        paths.sort();
        let mut count = 0;
        for path in paths {
            match std::fs::read_to_string(&path) {
                Ok(text) => count += self.add_file(&text, &path.display().to_string()),
                Err(e) => self.problems.push(format!("{}: {e}", path.display())),
            }
        }
        self.check();
        count
    }

    /// Removes templates that use missing templates or are otherwise
    /// unusable, recording why.
    pub fn check(&mut self) {
        let problems = model::check_references(&mut self.templates);
        self.problems.extend(problems);
    }

    /// Problems found while loading, one line each.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    pub fn get(&self, id: &str) -> Option<&Template> {
        self.templates.get(id)
    }

    /// Every template, including helpers that aren't listed.
    pub fn templates(&self) -> impl Iterator<Item = &Template> {
        self.templates.values()
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// Templates meant to be picked by people (not helpers), by group then name.
    pub fn list(&self) -> Vec<TemplateSummary> {
        let mut list: Vec<TemplateSummary> = self
            .templates
            .values()
            .filter(|t| t.listed)
            .map(Template::summary)
            .collect();
        list.sort_by(|a, b| (&a.group, &a.name).cmp(&(&b.group, &b.name)));
        list
    }

    /// Listed templates whose signatures match `data` (which starts where the
    /// structure would), most specific first.
    pub fn detect(&self, data: &[u8]) -> Vec<TemplateSummary> {
        let mut found: Vec<&Template> = self
            .templates
            .values()
            .filter(|t| t.listed && t.matches(data))
            .collect();
        found.sort_by(|a, b| {
            b.signature_strength()
                .cmp(&a.signature_strength())
                .then(a.name.cmp(&b.name))
        });
        found.into_iter().map(Template::summary).collect()
    }

    /// Decodes `data` with a template. `base` is the absolute offset of
    /// `data[0]`, used for the offsets in the result.
    pub fn decode(&self, id: &str, data: &[u8], base: u64) -> Result<Node, TemplateError> {
        let template = self
            .get(id)
            .ok_or_else(|| TemplateError::Unknown(id.to_owned()))?;
        Ok(decode::Decoder::new(&self.templates, data, base).decode_root(template))
    }
}
