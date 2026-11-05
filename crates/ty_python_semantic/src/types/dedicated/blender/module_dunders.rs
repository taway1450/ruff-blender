use ty_blender::AddonKind;
use ty_python_core::ProgramFile;

use crate::types::{KnownClass, Type};
use crate::{Db, ProgramEnvironment};

use super::addon_of_file;

/// The type of the implicit `__package__` of `file` when it belongs to an add-on, or `None` to keep
/// ty's own `str | None`.
///
/// Blender imports a legacy add-on as a top-level package named after its folder, so `__package__` is
/// known exactly: `"my_addon"` in `my_addon/__init__.py` and `my_addon/ops.py`, and `"my_addon.ui"` in
/// `my_addon/ui/panel.py`. A single-file add-on is a top-level module, whose `__package__` is `""`. An
/// extension is imported as `bl_ext.<repository>.<id>`, and the repository comes from the user's
/// preferences, so only `str` is known.
#[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn package_type<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Option<Type<'db>> {
    let addon = addon_of_file(db, file).as_ref()?;
    let path = file.file(db).path(db).as_system_path()?;
    match addon.kind {
        AddonKind::Extension => {
            Some(KnownClass::Str.to_instance(db, &ProgramEnvironment::from_file(file)))
        }
        AddonKind::SingleFile => {
            (path == addon.root.as_path()).then(|| Type::string_literal(db, ""))
        }
        AddonKind::LegacyPackage => {
            let sub_packages = path.parent()?.strip_prefix(&addon.root).ok()?;
            let package = std::iter::once(addon.name.as_str())
                .chain(
                    sub_packages
                        .components()
                        .map(|component| component.as_str()),
                )
                .collect::<Vec<_>>()
                .join(".");
            Some(Type::string_literal(db, package.as_str()))
        }
    }
}
