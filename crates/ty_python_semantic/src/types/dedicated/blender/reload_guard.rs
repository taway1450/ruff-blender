//! Names a module bound on its previous run, which Blender's reload guard reads, and the submodules a
//! package imports by name.
//!
//! ```python
//! if "bpy" in locals():
//!     import importlib
//!     importlib.reload(ops)  # `ops` is the module the `else` branch imported last time
//! else:
//!     from . import ops
//! ```

use ruff_db::parsed::ParsedModuleRef;
use ruff_python_ast as ast;
use ruff_text_size::Ranged;
use ty_blender::{imports_submodule_by_name, in_reload_guard};
use ty_module_resolver::{ModuleName, file_to_module, resolve_module_confident};
use ty_python_core::ProgramFile;
use ty_python_core::scope::ScopeId;

use crate::place::explicit_global_symbol;
use crate::types::Type;
use crate::{Db, ProgramEnvironment};

/// The type of `name`, unbound where it is read, when the module bound it on a previous run or
/// imports it by name. `None` keeps ty's `unresolved-reference`.
///
/// - In the guarded branch of a module-level reload guard: the module's global type of the name,
///   which a binding elsewhere in the module gave it on the previous run, or else, in a package's
///   `__init__`, its submodule of that name: importing the submodule anywhere (`from . import ops`
///   in `register()`) bound it in the package.
/// - Anywhere in a package's `__init__` that imports its submodules with
///   `__import__(name=__name__, fromlist=[...])`: the submodule the list names.
///
/// Both hold for any module Blender reloads, an add-on or one of its own startup scripts.
pub(crate) fn reload_guard_type<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    module: &ParsedModuleRef,
    name: &ast::ExprName,
) -> Option<Type<'db>> {
    let file = scope.program_file(db);
    if file.file(db).is_stub(db) {
        return None;
    }
    let body = &module.syntax().body;
    let id = name.id.as_str();
    if scope.file_scope_id(db).is_global() && in_reload_guard(body, name.range()) {
        return explicit_global_symbol(db, file, id)
            .place
            .ignore_possibly_undefined()
            .or_else(|| submodule_type(db, file, id));
    }
    if imports_submodule_by_name(body, id) {
        return submodule_type(db, file, id);
    }
    None
}

/// The submodule `name` of the package whose `__init__` is `file`, as `file` imports it.
fn submodule_type<'db>(db: &'db dyn Db, file: ProgramFile<'db>, name: &str) -> Option<Type<'db>> {
    let package = file_to_module(db, file.resolver_file(db))?;
    if !package.kind(db).is_package() {
        return None;
    }
    let mut module_name = package.name(db).clone();
    module_name.extend(&ModuleName::new(name)?);
    let env = ProgramEnvironment::from_file(file);
    let submodule = resolve_module_confident(db, env.resolver_environment(db), &module_name)?;
    Some(Type::module_literal(db, file, submodule))
}
