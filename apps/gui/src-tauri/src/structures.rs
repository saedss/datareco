//! Structure templates: listing, recognizing and decoding them, and the
//! folder for the user's own templates.

use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

use datareco_templates::{Node, TemplateSet, TemplateSummary};
use serde::Serialize;
use tauri::State;

use crate::{CommandError, OpenSources, read_range};

/// Bytes read when looking for a structure to suggest.
const DETECT_BYTES: u64 = 4096;

const README: &str = "\
DataReco templates folder

Put your own structure templates here as .toml files. DataReco loads them at
startup and when you click Reload in the Structure panel. A template with the
same id as a built-in one replaces it.

The format: https://github.com/saedss/datareco/blob/main/docs/TEMPLATES.md
The built-in templates make good examples:
https://github.com/saedss/datareco/tree/main/crates/templates/src/builtin
";

/// The loaded templates and where the user's own ones live.
pub struct Templates {
    set: RwLock<TemplateSet>,
    folder: PathBuf,
}

impl Templates {
    /// Loads the built-in templates plus the ones in `folder`, creating the
    /// folder (with a README) if it doesn't exist yet.
    pub fn load(folder: PathBuf) -> Self {
        prepare_folder(&folder);
        Self {
            set: RwLock::new(load_set(&folder)),
            folder,
        }
    }

    fn list(&self) -> TemplateList {
        let set = self.set.read().unwrap_or_else(PoisonError::into_inner);
        TemplateList {
            templates: set.list(),
            problems: set.problems().to_vec(),
            folder: self.folder.display().to_string(),
        }
    }
}

fn load_set(folder: &Path) -> TemplateSet {
    let mut set = TemplateSet::builtin();
    set.add_dir(folder);
    set
}

fn prepare_folder(folder: &Path) {
    // Best effort: without the folder, the built-in templates still work.
    if std::fs::create_dir_all(folder).is_ok() {
        let readme = folder.join("README.txt");
        if !readme.exists() {
            let _ = std::fs::write(readme, README);
        }
    }
}

/// Every listed template, problems loading the user's ones, and their folder.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateList {
    templates: Vec<TemplateSummary>,
    problems: Vec<String>,
    folder: String,
}

#[tauri::command]
pub fn list_templates(templates: State<'_, Templates>) -> TemplateList {
    templates.list()
}

/// Loads the templates again, picking up changes to the user's files.
#[tauri::command(async)]
pub fn reload_templates(templates: State<'_, Templates>) -> TemplateList {
    let fresh = load_set(&templates.folder);
    *templates
        .set
        .write()
        .unwrap_or_else(PoisonError::into_inner) = fresh;
    templates.list()
}

/// Templates whose signatures match the bytes at `offset`, best match first.
#[tauri::command(async)]
pub fn detect_templates(
    sources: State<'_, OpenSources>,
    templates: State<'_, Templates>,
    id: String,
    offset: u64,
) -> Result<Vec<TemplateSummary>, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let bytes = read_range(&disk, offset, DETECT_BYTES)?;
    let set = templates.set.read().unwrap_or_else(PoisonError::into_inner);
    Ok(set.detect(&bytes.data))
}

/// Decodes the structure at `offset` with a template.
#[tauri::command(async)]
pub fn decode_template(
    sources: State<'_, OpenSources>,
    templates: State<'_, Templates>,
    id: String,
    template: String,
    offset: u64,
) -> Result<Node, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let set = templates.set.read().unwrap_or_else(PoisonError::into_inner);
    let extent = set
        .get(&template)
        .ok_or_else(|| {
            CommandError::new(
                "template",
                format!("there is no template named \"{template}\""),
            )
        })?
        .extent;
    let bytes = read_range(&disk, offset, extent)?;
    set.decode(&template, &bytes.data, offset)
        .map_err(|e| CommandError::new("template", e.to_string()))
}

/// Opens the user's templates folder in the file manager.
#[tauri::command]
pub fn open_templates_folder(templates: State<'_, Templates>) -> Result<(), CommandError> {
    prepare_folder(&templates.folder);
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(&templates.folder)
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            CommandError::new(
                "io",
                format!("couldn't open {}: {e}", templates.folder.display()),
            )
        })
}
