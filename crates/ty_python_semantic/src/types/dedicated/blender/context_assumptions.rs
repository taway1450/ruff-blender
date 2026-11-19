//! The Assumptions about a context member that the code around the read takes for granted, with
//! nothing in the code to show it:
//!
//! - `blender-assumed-added-object`: `context.object` / `active_object` after a
//!   `bpy.ops.<category>.*_add()` call earlier in the same function or module-level script
//!   ([`ty_blender::adds_object`]), read as the Type tag view of the object the operator adds
//!   (`_Object_MESH` after `mesh.primitive_cube_add()`, [`ty_blender::added_object_type`]);
//! - `blender-assumed-editor-member`: `area`, `region` and `space_data` in a Panel's, Menu's or
//!   asset shelf's `poll`, a tool's `draw_settings` and `RenderEngine.view_update` / `view_draw`
//!   ([`ty_blender::assumes_editor`]);
//! - `blender-assumed-internal-operator`: any member in a callback (but `poll`) of an Internal
//!   operator, `'INTERNAL'` in its `bl_options`;
//! - `blender-assumed-preset-context`: any member read at module level in a preset, a file in a
//!   `presets` folder ([`ty_blender::is_preset_path`]). A preset whose panel shows only for one
//!   struct reads the member as that struct ([`ty_blender::preset_member_struct`]), and a pointer
//!   the panel's poll requires is read as set ([`ty_blender::preset_set_pointer`],
//!   [`on_preset_pointer_load`]).
//!
//! ```python
//! class MY_OT_show(Operator):
//!     bl_options = {'INTERNAL'}
//!
//!     def execute(self, context):
//!         context.screen.areas        # `Screen`: reported
//! ```
//!
//! They come last: a Guaranteed context member, a member a `poll()` checked and the
//! `scene` / `view_layer` / `tool_settings` / `collection` Assumption are applied first
//! ([`super::member::on_load`]). The read is typed without `None` and reported, unless the code
//! only tests it for truth or `None` or its own test already narrows it.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::comparable::ComparableExpr;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_blender::{
    added_object_type, adds_object, assumes_editor, callback_of, is_added_object_member,
    is_editor_member, is_preset_path, is_tool_draw_settings, preset_member_struct,
    preset_set_pointer,
};
use ty_python_core::scope::ScopeId;
use ty_python_core::{global_scope, semantic_index};

use crate::Db;
use crate::lint::LintMetadata;
use crate::place::PlaceAndQualifiers;
use crate::types::context::InferContext;
use crate::types::infer::nearest_enclosing_class;
use crate::types::{StaticClassLiteral, Type};

use super::classes::{mro_classes, own_binding};
use super::context_members::{enclosing_function, is_context_instance};
use super::lints::{
    BLENDER_ASSUMED_ADDED_OBJECT, BLENDER_ASSUMED_EDITOR_MEMBER, BLENDER_ASSUMED_INTERNAL_OPERATOR,
    BLENDER_ASSUMED_PRESET_CONTEXT,
};
use super::member::without_none;
use super::overrides::callback_root;
use super::poll_checks::{is_tested_read, single_comparison};
use super::symbols::{bpy_types_class_name, is_bpy_types_class};
use super::vector::module_class;

/// The member `attribute` read from `receiver`, where ty found `place`, typed without `None` and
/// reported when one of the Assumptions above takes it as set. `None` leaves the read as it is.
pub(super) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
    place: PlaceAndQualifiers<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    if !attribute.ctx.is_load() {
        return None;
    }
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    if !is_context_instance(db, env, receiver) {
        return None;
    }
    let original = place.place.ignore_possibly_undefined()?;
    let narrowed = without_none(db, original);
    if narrowed == original {
        return None;
    }
    let function = enclosing_function(db, file, attribute);
    let scope = function.unwrap_or_else(|| global_scope(db, file));
    let (rule, reason, struct_name) = assumption(db, scope, function.is_some(), attribute)?;
    // The object an operator adds is its Type tag's view; a preset whose panel shows only for one
    // struct reads the member as that struct.
    let narrowed = if let Some(struct_name) = struct_name
        && let Some(class) = module_class(db, env, "bpy.types", &struct_name)
    {
        Type::instance(db, env, class.default_specialization(db))
    } else if rule.name == BLENDER_ASSUMED_PRESET_CONTEXT.name
        && let Some(struct_name) =
            preset_member_struct(file.file(db).path(db).as_str(), attribute.attr.id.as_str())
        && let Some(class) = module_class(db, env, "bpy.types", struct_name)
    {
        Type::instance(db, env, class.default_specialization(db))
    } else {
        narrowed
    };
    if !is_tested_read(db, scope, attribute)
        && !is_guarded_by_operand(db, scope, attribute)
        && let Some(builder) = context.report_lint(rule, attribute)
    {
        builder.into_diagnostic(format_args!(
            "`context.{}` is taken as `{}`: {reason}",
            attribute.attr.id,
            narrowed.display(db, env),
        ));
    }
    Some(place.map_type(|_| narrowed))
}

/// The pointer `attribute` read at module level in a preset from an instance of a `bpy.types`
/// struct, typed without `None` and reported under `blender-assumed-preset-context` when the panel
/// that applies the preset shows only with it set: `material.grease_pencil` in a Grease Pencil
/// material preset. `None` leaves the read as it is.
pub(super) fn on_preset_pointer_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
    place: PlaceAndQualifiers<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    if !attribute.ctx.is_load() {
        return None;
    }
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let Type::NominalInstance(instance) = receiver else {
        return None;
    };
    let class = instance.class(db, env).class_literal(db).as_static()?;
    let struct_name = bpy_types_class_name(db, class)?;
    if enclosing_function(db, file, attribute).is_some()
        || !preset_set_pointer(
            file.file(db).path(db).as_str(),
            struct_name,
            attribute.attr.id.as_str(),
        )
    {
        return None;
    }
    let original = place.place.ignore_possibly_undefined()?;
    let narrowed = without_none(db, original);
    if narrowed == original {
        return None;
    }
    if !is_tested_read(db, global_scope(db, file), attribute)
        && let Some(builder) = context.report_lint(&BLENDER_ASSUMED_PRESET_CONTEXT, attribute)
    {
        builder.into_diagnostic(format_args!(
            "`{struct_name}.{}` is taken as `{}`: a preset runs from the panel that applies it, \
             which shows only when it is set",
            attribute.attr.id,
            narrowed.display(db, env),
        ));
    }
    Some(place.map_type(|_| narrowed))
}

/// The rule that takes the read `attribute` in `scope` (a function when `in_function`, else the
/// module) as set, why, and the `bpy.types` class it is read as when that is narrower than the
/// member's type.
fn assumption<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    in_function: bool,
    attribute: &ast::ExprAttribute,
) -> Option<(&'static LintMetadata, String, Option<String>)> {
    let name = attribute.attr.id.as_str();
    if is_added_object_member(name)
        && let Some(added) = added_object_calls(db, scope)
            .iter()
            .rev()
            .find(|added| added.call.end() <= attribute.start())
    {
        return Some((
            &BLENDER_ASSUMED_ADDED_OBJECT,
            format!(
                "`bpy.ops.{}()` before it is taken to have added an object and made it active",
                added.operator
            ),
            added.tag.as_ref().map(|tag| format!("_Object_{tag}")),
        ));
    }
    if !in_function {
        let file = scope.program_file(db);
        return is_preset_path(file.file(db).path(db).as_str()).then(|| {
            (
                &BLENDER_ASSUMED_PRESET_CONTEXT,
                "a preset runs with the context of the panel or menu that applies it".to_string(),
                None,
            )
        });
    }
    let function = function_name(db, scope)?;
    let callback = callback_class(db, scope, &function);
    if is_editor_member(name) {
        let reason = match callback {
            Some((_, root)) if assumes_editor(root, &function) => Some(format!(
                "Blender calls `{root}.{function}` while it draws an editor, though a script can \
                 call it with none"
            )),
            _ if is_tool_draw_settings(&function) => Some(
                "Blender calls a tool's `draw_settings` while it draws an editor's tool settings"
                    .to_string(),
            ),
            _ => None,
        };
        if let Some(reason) = reason {
            return Some((&BLENDER_ASSUMED_EDITOR_MEMBER, reason, None));
        }
    }
    let (class, root) = callback?;
    if root == "Operator" && function != "poll" && is_internal(db, class) {
        return Some((
            &BLENDER_ASSUMED_INTERNAL_OPERATOR,
            "the operator's `bl_options` has `'INTERNAL'`, so it is taken to run only from the UI \
             that offers it"
                .to_string(),
            None,
        ));
    }
    None
}

/// Whether the read `attribute` in `scope` is only tested within an expression, or is guarded by an
/// earlier operand: in `context.area is not None and context.area.type == 'VIEW_3D'` the first read
/// is only compared with `None`, and ty's own narrowing types the second. Neither takes anything
/// for granted. The statement-level tests are [`is_tested_read`]'s.
fn is_guarded_by_operand(db: &dyn Db, scope: ScopeId<'_>, attribute: &ast::ExprAttribute) -> bool {
    let file = scope.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let body = match scope.node(db).as_function() {
        Some(function) => &function.node(&module).body[..],
        None => &module.suite()[..],
    };
    let mut guards = Guards {
        read: attribute,
        guarded: false,
    };
    guards.visit_body(body);
    guards.guarded
}

/// Looks for a boolean operation around a read with an earlier operand that tests it.
struct Guards<'r> {
    read: &'r ast::ExprAttribute,
    guarded: bool,
}

impl Guards<'_> {
    /// Whether `expression` is the read itself.
    fn is_read(&self, expression: &Expr) -> bool {
        expression.range() == self.read.range()
            || expression.as_attribute_expr().is_some_and(|attribute| {
                attribute.attr.id == self.read.attr.id
                    && ComparableExpr::from(&*attribute.value)
                        == ComparableExpr::from(&*self.read.value)
            })
    }

    /// Whether `operand` of an `and` (`truthy`) or an `or` says that the read is not `None`.
    fn guards(&self, operand: &Expr, truthy: bool) -> bool {
        let is_read = |expression: &Expr| self.is_read(expression);
        match operand {
            Expr::BoolOp(ast::ExprBoolOp { op, values, .. })
                if (*op == ast::BoolOp::And) == truthy =>
            {
                values.iter().any(|value| self.guards(value, truthy))
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => self.guards(operand, !truthy),
            Expr::Compare(compare) => match single_comparison(compare) {
                Some((tested, op, other)) if other.is_none_literal_expr() && is_read(tested) => {
                    if truthy {
                        matches!(op, ast::CmpOp::IsNot | ast::CmpOp::NotEq)
                    } else {
                        matches!(op, ast::CmpOp::Is | ast::CmpOp::Eq)
                    }
                }
                _ => false,
            },
            _ => truthy && is_read(operand),
        }
    }
}

impl<'a> Visitor<'a> for Guards<'_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if !self.guarded
            && statement.range().contains_range(self.read.range())
            && !matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_))
        {
            walk_stmt(self, statement);
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        if self.guarded || !expression.range().contains_range(self.read.range()) {
            return;
        }
        match expression {
            // `X is None`, `X is not None`: the read is only tested.
            Expr::Compare(compare)
                if single_comparison(compare).is_some_and(|(tested, _, other)| {
                    other.is_none_literal_expr() && tested.range() == self.read.range()
                }) =>
            {
                self.guarded = true;
                return;
            }
            Expr::BoolOp(ast::ExprBoolOp { op, values, .. }) => {
                let truthy = *op == ast::BoolOp::And;
                let last = values.len().saturating_sub(1);
                for (position, value) in values.iter().enumerate() {
                    if value.range().contains_range(self.read.range()) {
                        // `X and ...`: an operand before the last one is only tested for truth.
                        if position < last && value.range() == self.read.range() {
                            self.guarded = true;
                            return;
                        }
                        break;
                    }
                    if self.guards(value, truthy) {
                        self.guarded = true;
                        return;
                    }
                }
            }
            _ => {}
        }
        walk_expr(self, expression);
    }
}

/// The name of the function scope `scope`.
fn function_name(db: &dyn Db, scope: ScopeId<'_>) -> Option<Name> {
    let file = scope.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let function = scope.node(db).as_function()?;
    Some(function.node(&module).name.id.clone())
}

/// The class and callback root of the function scope `scope` named `name`, when it is a Blender
/// callback: a method in the body of a class deriving from a callback root, named after one of the
/// root's callbacks.
fn callback_class<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    name: &str,
) -> Option<(StaticClassLiteral<'db>, &'db str)> {
    let file = scope.program_file(db);
    let index = semantic_index(db, file);
    let parent = index.parent_scope_id(scope.file_scope_id(db))?;
    index.scope(parent).node().as_class()?;
    let class = nearest_enclosing_class(db, index, scope)?;
    let root = callback_root(db, class)?;
    callback_of(root, name)?;
    Some((class, root))
}

/// Whether the operator `class` is an Internal operator: the nearest `bl_options` of its method
/// resolution order, before the `bpy.types` classes, is a set, list or tuple literal with
/// `'INTERNAL'` among its strings.
fn is_internal(db: &dyn Db, class: StaticClassLiteral<'_>) -> bool {
    let Some((definition, _)) = mro_classes(db, class)
        .take_while(|base| !is_bpy_types_class(db, *base))
        .find_map(|base| own_binding(db, base, "bl_options"))
    else {
        return false;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let elements = match definition.kind(db).value(&module) {
        Some(Expr::Set(set)) => &set.elts,
        Some(Expr::List(list)) => &list.elts,
        Some(Expr::Tuple(tuple)) => &tuple.elts,
        _ => return false,
    };
    elements.iter().any(|element| {
        element
            .as_string_literal_expr()
            .is_some_and(|string| string.value.to_str() == "INTERNAL")
    })
}

/// A call of an operator that adds an object.
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
struct AddedObjectCall {
    call: TextRange,
    /// `category.name` of the operator.
    operator: Name,
    /// The Type tag of the object it adds, when the call shows it.
    tag: Option<Name>,
}

/// The calls of an operator that adds an object (`bpy.ops.mesh.primitive_cube_add(...)`) directly in
/// the scope `scope`, a function or a module; by syntax alone, in source order, not those in a
/// nested function, class or lambda.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn added_object_calls<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> Box<[AddedObjectCall]> {
    let file = scope.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let body = match scope.node(db).as_function() {
        Some(function) => &function.node(&module).body[..],
        None if scope.file_scope_id(db).is_global() => &module.suite()[..],
        None => return Box::default(),
    };
    let mut calls = AddedObjectCalls::default();
    calls.visit_body(body);
    calls.0.into_boxed_slice()
}

#[derive(Default)]
struct AddedObjectCalls(Vec<AddedObjectCall>);

impl<'a> Visitor<'a> for AddedObjectCalls {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if !matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
            walk_stmt(self, statement);
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Lambda(_) => {}
            Expr::Call(call) => {
                walk_expr(self, expression);
                if let Some((category, name)) = added_object_operator(&call.func) {
                    let type_argument = call
                        .arguments
                        .find_keyword("type")
                        .and_then(|keyword| keyword.value.as_string_literal_expr())
                        .map(|string| string.value.to_str());
                    self.0.push(AddedObjectCall {
                        call: call.range(),
                        operator: Name::new(format!("{category}.{name}")),
                        tag: added_object_type(category, name, type_argument).map(Name::new_static),
                    });
                }
            }
            _ => walk_expr(self, expression),
        }
    }
}

/// `(category, name)` when `callee` is `bpy.ops.<category>.<name>` (or `ops.<category>.<name>`) of
/// an operator that adds an object.
fn added_object_operator(callee: &Expr) -> Option<(&str, &str)> {
    let Expr::Attribute(ast::ExprAttribute {
        value, attr: name, ..
    }) = callee
    else {
        return None;
    };
    let Expr::Attribute(ast::ExprAttribute {
        value: ops,
        attr: category,
        ..
    }) = &**value
    else {
        return None;
    };
    let is_ops = match &**ops {
        Expr::Name(ops) => ops.id == "ops",
        Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            attr.id == "ops" && value.as_name_expr().is_some_and(|bpy| bpy.id == "bpy")
        }
        _ => false,
    };
    (is_ops && adds_object(name.id.as_str())).then(|| (category.id.as_str(), name.id.as_str()))
}
