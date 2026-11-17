//! Reads that Blender's own bookkeeping narrows, wherever they are, without a `poll()`: the
//! `gpencil_settings` of a Grease Pencil paint mode's brush, never `None` once the brush is read
//! ([`is_set_once_read`]).
//!
//! ```python
//! context.tool_settings.gpencil_paint.brush.gpencil_settings.pen_strength  # not `None`
//! ```
//!
//! Blender's own invariant, so it is not reported. The Pointer aliases between poll facts
//! (`node_tree` and `edit_tree`) are applied by [`super::poll_checks`].

use ruff_python_ast as ast;
use ty_blender::is_set_once_read;
use ty_python_core::ProgramFile;

use crate::Db;
use crate::place::PlaceAndQualifiers;
use crate::types::Type;
use crate::types::context::InferContext;

use super::context_members::enclosing_function;
use super::member::without_none;
use super::poll_checks::function_paths;

/// The type of the attribute read `attribute`, where ty found `place`, when Blender's bookkeeping
/// narrows it.
pub(super) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    place: PlaceAndQualifiers<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    if !attribute.ctx.is_load() || attribute.attr.id != "gpencil_settings" {
        return None;
    }
    let db = context.db();
    let original = place.place.ignore_possibly_undefined()?;
    let narrowed = brush_settings(db, context.program_file(), attribute, original)?;
    (narrowed != original).then(|| place.map_type(|_| narrowed))
}

/// `<gp paint>.brush.gpencil_settings`, a path from the context: `original` without `None`.
fn brush_settings<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
    original: Type<'db>,
) -> Option<Type<'db>> {
    let function = enclosing_function(db, file, attribute)?;
    let path = function_paths(db, function).attribute_path(attribute)?;
    is_set_once_read(&path).then(|| without_none(db, original))
}
