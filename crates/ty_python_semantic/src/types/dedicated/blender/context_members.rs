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
//! `bpy.context` and an alias (`scene = context.scene`, since the read's own type changes).
//! `temp_override(scene=None)` empties the context: a known unsoundness, not special-cased.
//!
//! Two Implied narrowings give the space by convention rather than by guarantee, so their reads
//! are reported (the category `blender-implied-narrowings`):
//!
//! - a Menu has no `bl_space_type`, but in its `draw` and `poll` the editor prefix of its idname
//!   ([`ty_blender::menu_space_type`], `CLIP_MT_view`) gives `space_data` and `area`
//!   (`blender-narrowed-by-menu-prefix`);
//! - a Panel that the same add-on opens as a popover (`layout.popover(...)`, `wm.call_panel`) is
//!   drawn with the opening editor's area, so its `space_data` keeps the `bl_space_type` subclass
//!   but is reported (`blender-narrowed-by-popover`).

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{self as ast, Expr};
use ty_blender::{
    ContextGuarantee, OperatorIdname, callback_of, context_guarantee, is_assumed_context_member,
    menu_space_type, runs_in_editor,
};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, global_scope, place_table, semantic_index};

use crate::place::{PlaceAndQualifiers, imported_symbol};
use crate::types::context::InferContext;
use crate::types::infer::nearest_enclosing_class;
use crate::types::{StaticClassLiteral, Type};
use crate::{Db, ProgramEnvironment};

use super::classes::{class_string, mro_classes, operator_idname};
use super::index::addon_in_program;
use super::lints::{
    BLENDER_ASSUMED_CONTEXT_MEMBER, BLENDER_NARROWED_BY_MENU_PREFIX, BLENDER_NARROWED_BY_POPOVER,
};
use super::member::without_none;
use super::overrides::callback_root;
use super::poll_checks::is_tested_read;
use super::symbols::bpy_types_class_name;
use super::vector::module_class;

/// The member `attribute` read from `receiver` in the file of `context`, where ty found `place`,
/// when it is a Guaranteed context member of the Blender callback the read is in, or a space a
/// menu's idname prefix or a popover implies (reported to `context`). `None` leaves the read to the
/// other hooks.
pub(super) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
    place: PlaceAndQualifiers<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let name = attribute.attr.id.as_str();
    let guarantee = context_guarantee(name)?;
    if !is_context_instance(db, env, receiver) {
        return None;
    }
    let function = enclosing_function(db, file, attribute);
    let callback = function.and_then(|function| callback_context(db, function).as_ref());
    let Some(callback) = callback else {
        // Outside a callback: an Assumption for the members Blender keeps in ordinary use.
        return assumed_member(context, attribute, function, place, None);
    };
    let function = function?;
    match guarantee {
        ContextGuarantee::Always if callback.import_operator && is_assumed_in_import(name) => {
            assumed_member(context, attribute, Some(function), place, Some(callback))
        }
        ContextGuarantee::Always => Some(place.map_type(|ty| without_none(db, ty))),
        ContextGuarantee::InEditor => {
            let space_type = callback.space_type.as_deref()?;
            let rule = match (callback.space_source, name) {
                (SpaceSource::MenuPrefix, "region") => return None,
                (SpaceSource::MenuPrefix, _) => Some(&BLENDER_NARROWED_BY_MENU_PREFIX),
                (SpaceSource::Popover, "space_data") => Some(&BLENDER_NARROWED_BY_POPOVER),
                _ => None,
            };
            let space = (name == "space_data")
                .then(|| space_subclass(db, env, space_type))
                .flatten();
            let narrowed = place.map_type(|ty| space.unwrap_or_else(|| without_none(db, ty)));
            if let Some(rule) = rule
                && narrowed.place != place.place
                && !is_tested_read(db, function, attribute)
                && let Some(ty) = narrowed.place.ignore_possibly_undefined()
                && let Some(builder) = context.report_lint(rule, attribute)
            {
                let reason = match callback.space_source {
                    SpaceSource::Popover => format!(
                        "the panel's `bl_space_type` is `'{space_type}'`, but the add-on opens it as \
                         a popover, which is drawn with the area of the editor that opens it"
                    ),
                    _ => format!(
                        "the menu's idname prefix names the editor `'{space_type}'`, but a menu \
                         is drawn in whatever editor opens it"
                    ),
                };
                builder.into_diagnostic(format_args!(
                    "`context.{name}` is taken as `{}`: {reason}",
                    ty.display(db, env),
                ));
            }
            Some(narrowed)
        }
    }
}

/// The read `attribute` of an assumed context member ([`ty_blender::is_assumed_context_member`]),
/// in the function scope `function` (`None` at module level), outside a Blender callback or, with
/// `callback`, in a callback of a FileHandler's import operator: narrowed without `None`, and
/// reported under `blender-assumed-context-member` unless the code only tests it.
fn assumed_member<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    function: Option<ScopeId<'db>>,
    place: PlaceAndQualifiers<'db>,
    callback: Option<&CallbackContext>,
) -> Option<PlaceAndQualifiers<'db>> {
    let db = context.db();
    let env = context.program_environment();
    let name = attribute.attr.id.as_str();
    if !is_assumed_context_member(name) {
        return None;
    }
    let narrowed = place.map_type(|ty| without_none(db, ty));
    let scope = function.unwrap_or_else(|| global_scope(db, context.program_file()));
    if narrowed.place != place.place
        && !is_tested_read(db, scope, attribute)
        && let Some(ty) = narrowed.place.ignore_possibly_undefined()
        && let Some(builder) = context.report_lint(&BLENDER_ASSUMED_CONTEXT_MEMBER, attribute)
    {
        let reason = if callback.is_some() {
            "a FileHandler names this operator as its `bl_import_operator`, and Blender's collection \
             importer runs it with no scene"
        } else {
            "Blender guarantees it only while it calls a callback, though a timer, a handler or a \
             script has one in ordinary use"
        };
        builder.into_diagnostic(format_args!(
            "`context.{name}` is taken as `{}`: {reason}",
            ty.display(db, env),
        ));
    }
    Some(narrowed)
}

/// Whether `name` is a member that the collection importer leaves `None` in a FileHandler's import
/// operator: `scene` and the scene's `tool_settings`. `view_layer` crashes there rather than give
/// `None`.
fn is_assumed_in_import(name: &str) -> bool {
    matches!(name, "scene" | "tool_settings")
}

/// Whether `ty` is an instance of `bpy.types.Context`.
pub(super) fn is_context_instance<'db>(
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
        .is_some_and(|class| bpy_types_class_name(db, class) == Some("Context"))
}

/// The function scope `attribute` is read in: the nearest enclosing function, also from a lambda
/// or a comprehension inside it. A nested `def` is a function of its own: it may run after the
/// callback returns (a timer, a draw handler).
pub(super) fn enclosing_function<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
) -> Option<ScopeId<'db>> {
    let index = semantic_index(db, file);
    let scope = index.try_expression_scope_id(&*attribute.value)?;
    let function = index
        .ancestor_scopes(scope)
        .find_map(|(id, ancestor)| ancestor.node().as_function().map(|_| id))?;
    Some(function.to_scope_id(db, file))
}

/// What Blender fills in the context it passes to a callback.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
struct CallbackContext {
    /// The `bl_space_type` of the editor Blender calls the callback in: a literal one of a Panel,
    /// Header or GizmoGroup ([`ty_blender::runs_in_editor`]), or the one a menu's idname prefix
    /// names. `None` when the callback has no editor of a known type.
    space_type: Option<String>,
    space_source: SpaceSource,
    /// Whether the callback is an Operator's that a FileHandler of the same add-on names as its
    /// `bl_import_operator`.
    import_operator: bool,
}

/// Where the editor of a callback comes from.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
enum SpaceSource {
    /// The class's `bl_space_type`, which Blender guarantees.
    BlSpaceType,
    /// A menu's idname prefix, a convention.
    MenuPrefix,
    /// The `bl_space_type` of a panel the add-on also opens as a popover.
    Popover,
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
    let (space_type, space_source) = if runs_in_editor(root, name) {
        let space_type = bl_space_type(db, class).clone();
        let source = if root == "Panel" && space_type.is_some() && is_popover(db, file, class) {
            SpaceSource::Popover
        } else {
            SpaceSource::BlSpaceType
        };
        (space_type, source)
    } else if root == "Menu" && matches!(name, "draw" | "poll") {
        let idname =
            class_string(db, class, "bl_idname").unwrap_or_else(|| class.name(db).to_string());
        (
            menu_space_type(&idname).map(str::to_string),
            SpaceSource::MenuPrefix,
        )
    } else {
        (None, SpaceSource::BlSpaceType)
    };
    let import_operator = root == "Operator" && is_import_operator(db, file, class);
    Some(CallbackContext {
        space_type,
        space_source,
        import_operator,
    })
}

/// Whether the panel `class`, defined in `file`, is opened as a popover by its add-on: its idname
/// (`bl_idname`, or without one its class name) is the string literal panel name of a
/// `popover(...)` call or a `call_panel(name=...)` in one of the add-on's files (or in `file`, when
/// it belongs to no add-on).
pub(super) fn is_popover<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
) -> bool {
    let idname = class_string(db, class, "bl_idname").unwrap_or_else(|| class.name(db).to_string());
    let opens = |file| {
        popover_panels(db, file)
            .iter()
            .any(|panel| *panel == idname)
    };
    match addon_in_program(db, file) {
        Some(addon) => addon.files(db).any(opens),
        None => opens(file),
    }
}

/// Whether the operator `class`, defined in `file`, is the import operator of a FileHandler of its
/// add-on (or of `file`, when it belongs to no add-on): a class whose `bl_import_operator` is a
/// string literal naming the operator's idname, in either form.
fn is_import_operator<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
) -> bool {
    let Some(idname) = operator_idname(db, class) else {
        return false;
    };
    let names = |file| {
        import_operators(db, file)
            .iter()
            .filter_map(|operator| OperatorIdname::from_lookup(operator))
            .any(|operator| operator == idname)
    };
    match addon_in_program(db, file) {
        Some(addon) => addon.files(db).any(names),
        None => names(file),
    }
}

/// The string literal `bl_import_operator` values of the module-level classes of `file`, by syntax
/// alone.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn import_operators<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Box<[Name]> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    let mut operators = Vec::new();
    for statement in module.suite() {
        let ast::Stmt::ClassDef(class) = statement else {
            continue;
        };
        for statement in &class.body {
            let (target, value) = match statement {
                ast::Stmt::Assign(assign) => match &*assign.targets {
                    [target] => (target, Some(&*assign.value)),
                    _ => continue,
                },
                ast::Stmt::AnnAssign(assign) => (&*assign.target, assign.value.as_deref()),
                _ => continue,
            };
            if let (Expr::Name(target), Some(Expr::StringLiteral(value))) = (target, value)
                && target.id == "bl_import_operator"
            {
                operators.push(Name::new(value.value.to_str()));
            }
        }
    }
    operators.into_boxed_slice()
}

/// The panel idnames `file` opens as a popover, by syntax alone: the string literal panel of
/// `layout.popover("MY_PT_panel")` or `popover(panel="MY_PT_panel")`, and the `name` of
/// `bpy.ops.wm.call_panel(name="MY_PT_panel")`.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn popover_panels<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Box<[Name]> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    let mut finder = PopoverPanels::default();
    finder.visit_body(module.suite());
    finder.panels.into_boxed_slice()
}

#[derive(Default)]
struct PopoverPanels {
    panels: Vec<Name>,
}

impl Visitor<'_> for PopoverPanels {
    fn visit_expr(&mut self, expression: &Expr) {
        if let Expr::Call(call) = expression
            && let Expr::Attribute(callee) = &*call.func
        {
            let keyword = |name: &str| {
                call.arguments
                    .find_keyword(name)
                    .map(|keyword| &keyword.value)
            };
            let panel = match callee.attr.id.as_str() {
                "popover" => call.arguments.args.first().or_else(|| keyword("panel")),
                "call_panel" => keyword("name"),
                _ => None,
            };
            if let Some(Expr::StringLiteral(panel)) = panel
                && !self
                    .panels
                    .iter()
                    .any(|known| known == panel.value.to_str())
            {
                self.panels.push(Name::new(panel.value.to_str()));
            }
        }
        walk_expr(self, expression);
    }
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
pub(super) fn space_subclass<'db>(
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
