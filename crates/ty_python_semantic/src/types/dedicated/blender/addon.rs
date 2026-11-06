use ty_blender::{Addon, addon_for_path};
use ty_python_core::ProgramFile;

use crate::Db;

/// The add-on `file` belongs to, or `None` for a file in no add-on.
///
/// The probe is bounded by the outermost first-party root that contains the file, so a file outside
/// every first-party root (site-packages, typeshed, the stubs) never belongs to an add-on.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn addon_of_file<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Option<Addon> {
    let path = file.file(db).path(db).as_system_path()?;
    let bound = file
        .program(db)
        .search_paths(db)
        .first_party_roots()
        .filter(|root| path.starts_with(root))
        .min_by_key(|root| root.components().count())?;
    addon_for_path(db, path, bound)
}

/// Whether `file` is a Python source file in one of the program's first-party roots: the project's
/// own code, not typeshed, site-packages or the stubs.
///
/// A stub file (`.pyi`) never is, even inside a first-party root: Blender never loads one, and a
/// root often holds stubs, such as a `.venv` with the bpy stubs installed, or a `typings` folder on
/// an extra search path.
pub(super) fn is_first_party<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> bool {
    if file.file(db).is_stub(db) {
        return false;
    }
    file.file(db).path(db).as_system_path().is_some_and(|path| {
        file.program(db)
            .search_paths(db)
            .first_party_roots()
            .any(|root| path.starts_with(root))
    })
}
