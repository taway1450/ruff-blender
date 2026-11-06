//! Guaranteed context members: `context.scene` and its siblings are never `None` inside a Blender
//! callback, and `context.space_data` is the editor's `Space` subclass in a Panel, Header or
//! GizmoGroup of a known `bl_space_type`.
//!
//! The stubs type every pointer member of `bpy.types.Context` `X | None`, which is right for a
//! context in general: a helper may get a restricted or overridden one. Blender's guarantee holds
//! only while it calls a callback ([`ty_blender::context_guarantee`]), so a read is narrowed when
//! its receiver is a `Context` instance and it is lexically inside a Blender callback:
//!
//! ```python
//! class MY_PT_panel(Panel):
//!     bl_space_type = "VIEW_3D"
//!
//!     def draw(self, context):
//!         context.scene.render         # `Scene`, not `Scene | None`
//!         context.space_data.shading   # `SpaceView3D`
//!         scene = bpy.context.scene    # the same context: `Scene`
//!
//! def helper(context: Context):
//!     context.scene                    # `Scene | None`: Blender does not call it
//! ```
//!
//! Keying on the receiver's type, not on the `context` parameter, covers a renamed parameter,
//! `bpy.context` and an alias (`scene = context.scene`, since the read's own type changes). A Panel
//! opened as a popover gets the opening editor's area, and `temp_override(scene=None)` empties the
//! context: both are known unsoundnesses, not special-cased.

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::name::Name;
use ty_blender::{ContextGuarantee, callback_of, context_guarantee, runs_in_editor};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, global_scope, place_table, semantic_index};

use crate::place::{PlaceAndQualifiers, imported_symbol};
use crate::types::infer::nearest_enclosing_class;
use crate::types::{StaticClassLiteral, Type};
use crate::{Db, ProgramEnvironment};

use super::classes::{class_string, mro_classes};
use super::member::without_none;
use super::overrides::callback_root;
use super::symbols::bpy_types_class_name;
use super::vector::module_class;

/// The member `attribute` read from `receiver` in `file`, where ty found `place`, when it is a
/// Guaranteed context member of the Blender callback the read is in. `None` leaves the read to the
/// other hooks.
pub(super) fn on_load<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
    place: PlaceAndQualifiers<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    let name = attribute.attr.id.as_str();
    let guarantee = context_guarantee(name)?;
    if !is_context_instance(db, env, receiver) {
        return None;
    }
    let callback = enclosing_callback(db, file, attribute)?;
    match guarantee {
        ContextGuarantee::Always => Some(place.map_type(|ty| without_none(db, ty))),
        ContextGuarantee::InEditor => {
            let space_type = callback.space_type.as_deref()?;
            let space = (name == "space_data")
                .then(|| space_subclass(db, env, space_type))
                .flatten();
            Some(place.map_type(|ty| space.unwrap_or_else(|| without_none(db, ty))))
        }
    }
}

/// Whether `ty` is an instance of `bpy.types.Context`.
fn is_context_instance<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> bool {
    let Type::NominalInstance(instance) = ty else {
        return false;
    };
    instance
        .class(db, env)
        .class_literal(db)
        .as_static()
        .is_some_and(|class| bpy_types_class_name(db, class) == Some("Context"))
}

/// The Blender callback that `attribute` is read in: the nearest enclosing function, when it is a
/// callback, also from a lambda or a comprehension inside it. A nested `def` is no callback: it may
/// run after the callback returns (a timer, a draw handler).
fn enclosing_callback<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
) -> Option<&'db CallbackContext> {
    let index = semantic_index(db, file);
    let scope = index.try_expression_scope_id(&*attribute.value)?;
    let function = index
        .ancestor_scopes(scope)
        .find_map(|(id, ancestor)| ancestor.node().as_function().map(|_| id))?;
    callback_context(db, function.to_scope_id(db, file)).as_ref()
}

/// What Blender fills in the context it passes to a callback.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
struct CallbackContext {
    /// The `bl_space_type` of the editor Blender calls the callback in: a literal one of a Panel,
    /// Header or GizmoGroup ([`ty_blender::runs_in_editor`]). `None` when the callback has no
    /// editor of a known type.
    space_type: Option<String>,
}

/// The context Blender passes when it calls the function scope `function`, when that is a Blender
/// callback: a method in the body of a class deriving from a callback root, named after one of the
/// root's callbacks. It is found by its name, not by registration: an unregistered base's `draw` is
/// inherited by the registered subclass that Blender calls it on.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn callback_context<'db>(db: &'db dyn Db, function: ScopeId<'db>) -> Option<CallbackContext> {
    let file = function.program_file(db);
    let index = semantic_index(db, file);
    let in_class_body = index
        .parent_scope_id(function.file_scope_id(db))
        .is_some_and(|parent| index.scope(parent).node().as_class().is_some());
    let node = function.node(db).as_function().filter(|_| in_class_body)?;
    let module = parsed_module(db, file.python_file(db)).load(db);
    let name = node.node(&module).name.as_str();
    let class = nearest_enclosing_class(db, index, function)?;
    let root = callback_root(db, class)?;
    callback_of(root, name)?;
    let space_type = runs_in_editor(root, name)
        .then(|| bl_space_type(db, class).clone())
        .flatten();
    Some(CallbackContext { space_type })
}

/// The `bl_space_type` of `class` when it is a string literal, as Blender reads it from the class
/// and the classes it derives from.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn bl_space_type<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> Option<String> {
    class_string(db, class, "bl_space_type")
}

/// The `Space` subclass of the editor `space_type` (`SpaceView3D` for `'VIEW_3D'`), as an
/// instance. `None` when no subclass declares that Type tag: the top bar, the status bar and an
/// empty area have no subclass of their own, and stubs without Type tags declare none.
fn space_subclass<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    space_type: &str,
) -> Option<Type<'db>> {
    let space = module_class(db, env, "bpy.types", "Space")?;
    let (_, class) = space_subclasses(db, space.program_file(db))
        .iter()
        .find(|(tag, _)| tag == space_type)?;
    Some(Type::instance(db, env, class.default_specialization(db)))
}

/// The subclasses of `bpy.types.Space` in the `bpy.types` stub `file`, by their Type tag: the
/// string literal their `type` is declared as (`SpaceView3D.type: Literal['VIEW_3D']`, or a
/// property returning it).
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn space_subclasses<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Box<[(Name, StaticClassLiteral<'db>)]> {
    let env = ProgramEnvironment::from_file(file);
    let Some(space) = module_class(db, &env, "bpy.types", "Space") else {
        return Box::default();
    };
    place_table(db, global_scope(db, file))
        .symbols()
        .filter(|symbol| symbol.is_bound())
        .filter_map(|symbol| {
            let Type::ClassLiteral(class) =
                imported_symbol(db, &env, Some(file), symbol.name(), None)
                    .place
                    .ignore_possibly_undefined()?
            else {
                return None;
            };
            let class = class.as_static()?;
            if class == space || !mro_classes(db, class).any(|base| base == space) {
                return None;
            }
            let instance = Type::instance(db, &env, class.default_specialization(db));
            let tag = instance
                .member(db, &env, "type")
                .place
                .ignore_possibly_undefined()?
                .as_literal_value()?
                .as_string()?;
            Some((Name::new(tag.value(db)), class))
        })
        .collect()
}
