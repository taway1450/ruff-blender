//! Recognising Blender's own symbols by what they resolve to, never by how they are spelled.
//!
//! A symbol is Blender's when it is defined in a Blender module (`bpy.props`, `bpy.types`, ...)
//! that ty found on a search path other than a first-party root, which is where the bpy stubs live.
//! So `from bpy.props import IntProperty as IP`, `import bpy.props as p` and a star import all
//! resolve to the same function, while an add-on's own `def IntProperty(**kwargs)` or a
//! `bpy/props.py` inside the project never does.

use ty_module_resolver::file_to_module;
use ty_python_core::ProgramFile;

use crate::Db;
use crate::types::class_base::ClassBase;
use crate::types::{StaticClassLiteral, Type};

/// The name of the module `file` is, when it is a `bpy` module of Blender's stubs.
#[salsa::tracked(returns(as_deref), heap_size=ruff_memory_usage::heap_size)]
fn blender_module_name<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Option<Box<str>> {
    let module = file_to_module(db, file.resolver_file(db))?;
    if module.search_path(db)?.is_first_party() {
        return None;
    }
    let name = module.name(db).as_str();
    (name == "bpy" || name.starts_with("bpy.")).then(|| Box::from(name))
}

/// The name of the function `ty` is, when it is a function defined in the Blender module
/// `module` (`"bpy.props"`).
pub(super) fn blender_function_name<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
    module: &str,
) -> Option<&'db str> {
    let Type::FunctionLiteral(function) = ty else {
        return None;
    };
    (blender_module_name(db, function.program_file(db))? == module)
        .then(|| function.name(db).as_str())
}

/// The name of `class` when it is a class of `bpy.types`.
fn bpy_types_class_name<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> Option<&'db str> {
    (blender_module_name(db, class.program_file(db))? == "bpy.types")
        .then(|| class.name(db).as_str())
}

/// Whether `class` is a class of `bpy.types`.
pub(super) fn is_bpy_types_class<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    bpy_types_class_name(db, class).is_some()
}

/// The names of the `bpy.types` classes in the method resolution order of `class`, starting with
/// the most derived. A class defined in an add-on is never one of them.
pub(super) fn bpy_types_bases<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> impl Iterator<Item = &'db str> {
    class.iter_mro(db, None).filter_map(move |base| match base {
        ClassBase::Class(class_type) => {
            bpy_types_class_name(db, class_type.class_literal(db).as_static()?)
        }
        _ => None,
    })
}
