//! Names used in an add-on's reload guard, which Blender runs only when the module's names from the
//! previous import are still bound.
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
use ty_blender::in_reload_guard;
use ty_python_core::scope::ScopeId;

use crate::Db;
use crate::place::explicit_global_symbol;
use crate::types::Type;

use super::addon_of_file;

/// The type of `name`, unbound where it is used, when the use is in the guarded branch of a reload
/// guard of an add-on module: the module's global type of the name, which a binding elsewhere in the
/// module gave it on the previous import. `None` keeps ty's `unresolved-reference`.
pub(crate) fn reload_guard_type<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    module: &ParsedModuleRef,
    name: &ast::ExprName,
) -> Option<Type<'db>> {
    let file = scope.program_file(db);
    if !scope.file_scope_id(db).is_global()
        || addon_of_file(db, file).is_none()
        || !in_reload_guard(&module.syntax().body, name.range())
    {
        return None;
    }
    explicit_global_symbol(db, file, name.id.as_str())
        .place
        .ignore_possibly_undefined()
}
