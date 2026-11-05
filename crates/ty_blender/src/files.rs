//! The Python files of an add-on.

use ruff_db::Db;
use ruff_db::files::{File, directory_listing, system_path_to_file};
use ruff_db::system::SystemPath;

use crate::discovery::{Addon, AddonKind, existing_file, folder_addon};

/// The `.py` files of `addon`, in a fixed order: the files of a folder by name, then its sub-folders by
/// name.
///
/// The add-on root is walked as a folder tree rather than by following imports, because some files are
/// never imported (scripts, checks) and an add-on may load modules dynamically. The walk skips
/// `__pycache__` and dot-folders and stops at nested add-on roots (a folder with a manifest or a
/// `bl_info` package), whose files belong to the nested add-on. Roots listed in a `[tool.blender]`
/// table are not looked for. A single-file add-on is just its file.
///
/// Not cached here: the caller caches what it derives from the files per add-on. Every read goes through
/// the db, so a caller's query is invalidated when a file or folder of the add-on is added or removed.
pub fn addon_source_files(db: &dyn Db, addon: &Addon) -> Vec<File> {
    let mut files = Vec::new();
    match addon.kind {
        AddonKind::SingleFile => files.extend(existing_file(db, &addon.root)),
        AddonKind::Extension | AddonKind::LegacyPackage => {
            collect_folder(db, &addon.root, &mut files);
        }
    }
    files
}

fn collect_folder(db: &dyn Db, folder: &SystemPath, files: &mut Vec<File>) {
    let Ok(listing) = directory_listing(db, folder) else {
        return;
    };
    let mut sub_folders = Vec::new();
    for (name, _) in listing.iter() {
        if listing.entry_is_file(db, folder, name) {
            let path = folder.join(name);
            if path.extension() == Some("py")
                && let Ok(file) = system_path_to_file(db, path)
            {
                files.push(file);
            }
        } else if !name.starts_with('.')
            && name != "__pycache__"
            && listing.entry_is_directory(db, folder, name)
        {
            sub_folders.push(folder.join(name));
        }
    }
    for sub_folder in sub_folders {
        if folder_addon(db, &sub_folder).is_none() {
            collect_folder(db, &sub_folder, files);
        }
    }
}
