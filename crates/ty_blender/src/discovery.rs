//! Which add-on a file belongs to.
//!
//! - An add-on is *(kind, root, name)*: an extension (a folder with `blender_manifest.toml`), a legacy
//!   package (a folder whose `__init__.py` has a top-level `bl_info`) or a single file with `bl_info`.
//! - The probe walks up from the file's folder; the nearest root wins, so nested add-ons own their files.
//!   A single file counts only when no folder root encloses it.
//! - A `[tool.blender]` table in the nearest `pyproject.toml` replaces the probe in its folder tree.
//! - Every read goes through the db (`system_path_to_file`, `source_text`), so the callers' queries are
//!   invalidated when a manifest, `bl_info` or `pyproject.toml` changes, appears or disappears.

use ruff_db::Db;
use ruff_db::files::{File, system_path_to_file};
use ruff_db::system::{SystemPath, SystemPathBuf};

use crate::metadata::{MANIFEST_FILE, manifest, tool_blender_addons};
use crate::source::bl_info;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub enum AddonKind {
    Extension,
    LegacyPackage,
    SingleFile,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub struct Addon {
    pub kind: AddonKind,
    /// The add-on's folder, or the file itself for a single-file add-on.
    pub root: SystemPathBuf,
    /// The name Blender knows the add-on by (not ty's module name).
    pub name: String,
}

impl Addon {
    /// The module Blender imports: the package's `__init__.py`, or the single file.
    pub fn root_module(&self) -> SystemPathBuf {
        match self.kind {
            AddonKind::SingleFile => self.root.clone(),
            AddonKind::Extension | AddonKind::LegacyPackage => self.root.join("__init__.py"),
        }
    }

    /// The manifest of an extension, if the file exists.
    pub fn manifest_file(&self, db: &dyn Db) -> Option<File> {
        match self.kind {
            AddonKind::Extension => existing_file(db, &self.root.join(MANIFEST_FILE)),
            AddonKind::LegacyPackage | AddonKind::SingleFile => None,
        }
    }
}

/// The add-on `path` belongs to. `bound` is the outermost folder searched: the outermost first-party root
/// that contains `path`. Returns `None` for a file in no add-on.
pub fn addon_for_path(db: &dyn Db, path: &SystemPath, bound: &SystemPath) -> Option<Addon> {
    let start = path.parent()?;
    let folders = || {
        start
            .ancestors()
            .take_while(|folder| folder.starts_with(bound))
    };

    if let Some((pyproject_folder, listed)) = folders().find_map(|folder| {
        let pyproject = existing_file(db, &folder.join("pyproject.toml"))?;
        tool_blender_addons(db, pyproject)
            .as_ref()
            .map(|listed| (folder, listed))
    }) {
        return listed
            .iter()
            .map(|root| pyproject_folder.join(root))
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .map(|root| extension(db, root));
    }

    if let Some(addon) = folders().find_map(|folder| folder_addon(db, folder)) {
        return Some(addon);
    }

    let is_single_file = path.extension() == Some("py")
        && path.file_name() != Some("__init__.py")
        && existing_file(db, path).is_some_and(|file| bl_info(db, file).is_some());
    is_single_file.then(|| Addon {
        kind: AddonKind::SingleFile,
        root: path.to_path_buf(),
        name: path.file_stem().unwrap_or_default().to_string(),
    })
}

/// The add-on rooted at `folder`, when it has a manifest that is not a theme's (an extension) or an
/// `__init__.py` with a top-level `bl_info` (a legacy package).
pub(crate) fn folder_addon(db: &dyn Db, folder: &SystemPath) -> Option<Addon> {
    if let Some(manifest_file) = existing_file(db, &folder.join(MANIFEST_FILE)) {
        return (!manifest(db, manifest_file).is_theme())
            .then(|| extension(db, folder.to_path_buf()));
    }
    existing_file(db, &folder.join("__init__.py"))
        .is_some_and(|init| bl_info(db, init).is_some())
        .then(|| Addon {
            kind: AddonKind::LegacyPackage,
            root: folder.to_path_buf(),
            name: folder.file_name().unwrap_or_default().to_string(),
        })
}

/// An extension rooted at `root`: named by its manifest `id`, or by the folder when `id` is missing or invalid.
fn extension(db: &dyn Db, root: SystemPathBuf) -> Addon {
    let name = existing_file(db, &root.join(MANIFEST_FILE))
        .and_then(|file| manifest(db, file).valid_id().map(str::to_string))
        .unwrap_or_else(|| root.file_name().unwrap_or_default().to_string());
    Addon {
        kind: AddonKind::Extension,
        root,
        name,
    }
}

pub(crate) fn existing_file(db: &dyn Db, path: &SystemPath) -> Option<File> {
    system_path_to_file(db, path).ok()
}
