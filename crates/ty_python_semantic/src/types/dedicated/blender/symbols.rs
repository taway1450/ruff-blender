//! Recognising Blender's own symbols by what they resolve to, never by how they are spelled.
//!
//! A symbol is Blender's when it is defined in a Blender module (`bpy.props`, `bpy.types`,
//! `bpy_extras.io_utils`, ...)
//! that ty found on a search path other than a first-party root, which is where the bpy stubs live.
//! So `from bpy.props import IntProperty as IP`, `import bpy.props as p` and a star import all
//! resolve to the same function, while an add-on's own `def IntProperty(**kwargs)` or a
//! `bpy/props.py` inside the project never does.

use ty_module_resolver::file_to_module;
use ty_python_core::ProgramFile;

use crate::types::class_base::ClassBase;
use crate::types::{StaticClassLiteral, Type};
use crate::{Db, ProgramEnvironment};

/// The name of the module `file` is, when it is a `bpy` or `bpy_extras` module of Blender's stubs.
#[salsa::tracked(returns(as_deref), heap_size=ruff_memory_usage::heap_size)]
fn blender_module_name<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Option<Box<str>> {
    let module = file_to_module(db, file.resolver_file(db))?;
    if module.search_path(db)?.is_first_party() {
        return None;
    }
    let name = module.name(db).as_str();
    ["bpy", "bpy_extras"]
        .iter()
        .any(|package| {
            name.strip_prefix(package)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
        .then(|| Box::from(name))
}

/// The types that `bpy.types` holds in its `__dict__` from the start. Every other class of the
/// module is looked up in RNA by a module-level `__getattr__` when it is first used, and the module
/// has no `__all__`, so `from bpy.types import *` binds only these.
const BPY_TYPES_STAR_EXPORTS: [&str; 7] = [
    "bpy_struct_meta_idprop",
    "bpy_struct",
    "bpy_prop",
    "bpy_prop_array",
    "bpy_prop_collection",
    "bpy_prop_collection_idprop",
    "bpy_func",
];

/// Whether `from <file> import *` leaves `name` unbound although the stub defines it: true for
/// every name of Blender's `bpy.types` but its base types.
///
/// The stubs can't say this themselves. An `__all__` would also be what auto-import offers, and
/// `PropertyGroup` or `Operator` would no longer be suggested from `bpy.types`.
pub(crate) fn hides_star_import<'db>(db: &'db dyn Db, file: ProgramFile<'db>, name: &str) -> bool {
    blender_module_name(db, file) == Some("bpy.types") && !BPY_TYPES_STAR_EXPORTS.contains(&name)
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

/// Whether `ty` is an instance of `bpy.props._PropertyDeferred`, what the stubs say every
/// `bpy.props` function returns.
pub(super) fn is_property_deferred<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    let Type::NominalInstance(instance) = ty else {
        return false;
    };
    instance
        .class(db, env)
        .class_literal(db)
        .as_static()
        .is_some_and(|class| {
            class.name(db).as_str() == "_PropertyDeferred"
                && blender_module_name(db, class.program_file(db)) == Some("bpy.props")
        })
}
