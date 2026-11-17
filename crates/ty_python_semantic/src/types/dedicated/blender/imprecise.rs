//! Imprecise annotations: code less precise than the stubs that is right in Blender, accepted, typed
//! as the precise form and reported under a `blender-imprecise-*` rule (the category
//! `blender-imprecise-annotations`, [`super::categories`]).
//!
//! The stubs spell Blender's enums as `Literal` types, which exist nowhere at runtime: Blender checks
//! an enum name itself and raises for one that is not an item. So code that holds a name in a `str`
//! is right as long as the name is, and the rule's level only decides what is reported:
//!
//! ```python
//! class MY_OT_run(Operator):
//!     def execute(self, context) -> set[str]:  # blender-imprecise-operator-return
//!         return {"FINSHED"}  # error: [invalid-explicit-return], checked as the parent's result type
//!
//! def set_mode(mode: str) -> None:
//!     bpy.ops.object.mode_set(mode=mode)  # blender-imprecise-enum-value
//! ```
//!
//! - `blender-imprecise-operator-return`: a callback annotated with its parent's enum-set result with
//!   the names widened to `str` (`-> set[str]` for `set[Literal[...]]`) has the parent's result type
//!   ([`return_annotation`] hides the annotation, [`super::overrides::apply_override_types`] gives
//!   callers the parent's type), and so has a mixin's method that Blender calls
//!   ([`super::overrides::override_contract`] accepts it). A returned `set[str]` that is no set display
//!   (a set held in a variable) is accepted where the result is an enum set.
//! - `blender-imprecise-enum-value`: a `str` value (a `set[str]` for an enum flag set) where a
//!   Blender function, operator or property takes an enum `Literal` ([`accepts_enum_argument`],
//!   [`check_enum_write`]).
//! - `blender-imprecise-callback-parameter`: a callback parameter annotated with a subtype of the
//!   parent's type without `None` ([`narrowed_parameters`]), which only Blender calls, with what the
//!   add-on gave it.
//!
//! A string literal is never imprecise: `{"FINSHED"}` or `mode="OBJCT"` is a `Literal` that is not
//! one of the enum's, and stays an error.

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::helpers::ReturnStatementVisitor;
use ruff_python_ast::statement_visitor::StatementVisitor;
use ruff_text_size::{Ranged, TextRange};
use ty_blender::callbacks_of;
use ty_python_core::ProgramFile;
use ty_python_core::definition::{Definition, DefinitionKind};

use crate::types::call::Bindings;
use crate::types::class_base::ClassBase;
use crate::types::context::InferContext;
use crate::types::diagnostic::INVALID_ARGUMENT_TYPE;
use crate::types::signatures::function_signature_expression_type;
use crate::types::{KnownClass, MemberLookupPolicy, StaticClassLiteral, Type};
use crate::{Db, ProgramEnvironment};

use super::index::file_facts;
use super::lints::{
    BLENDER_IMPRECISE_CALLBACK_PARAMETER, BLENDER_IMPRECISE_ENUM_VALUE,
    BLENDER_IMPRECISE_OPERATOR_RETURN,
};
use super::member::without_none;
use super::operators::is_operator_call;
use super::overrides::{
    callback_root, defining_class, is_string_literals, override_types, parent_callback_types,
};
use super::symbols::is_blender_stub_file;

/// The item type of `ty` when it is an enum set: a `set`, `frozenset` or abstract `Set` of string
/// literals.
fn enum_set_item<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<Type<'db>> {
    let (class, specialization) = ty.class_specialization(db, env)?;
    let is_set = class.is_known(db, KnownClass::Set)
        || class.is_known(db, KnownClass::FrozenSet)
        || class.name(db).as_str() == "AbstractSet";
    let [item] = specialization.types(db) else {
        return None;
    };
    (is_set && is_string_literals(db, *item)).then_some(*item)
}

/// Whether `ty` is `set[str]`.
fn is_str_set<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> bool {
    ty.class_specialization(db, env)
        .is_some_and(|(class, specialization)| {
            class.is_known(db, KnownClass::Set)
                && specialization.types(db) == [KnownClass::Str.to_instance(db, env)]
        })
}

/// Whether `annotation` is `precise`, an enum set, with the names widened to `str`: `set[str]` for
/// `set[Literal["FINISHED", ...]]`.
fn widens_enum_set<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    annotation: Type<'db>,
    precise: Type<'db>,
) -> bool {
    is_str_set(db, env, annotation) && enum_set_item(db, env, precise).is_some()
}

/// The type of the return annotation of the function `definition`, if it has one.
#[salsa::tracked(
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn annotated_return<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<Type<'db>> {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let returns = function.node(&module).returns.as_deref()?;
    Some(function_signature_expression_type(db, definition, returns))
}

/// Whether the method `definition`, an override of a Blender callback, is annotated with its parent's
/// enum-set result widened to `str` (`def execute(self, context) -> set[str]`). Such a method is
/// typed as if it had no return annotation: it has the parent's result type.
pub(crate) fn has_imprecise_return<'db>(db: &'db dyn Db, definition: Definition<'db>) -> bool {
    let Some(types) = override_types(db, definition) else {
        return false;
    };
    let env = ProgramEnvironment::from_file(definition.program_file(db));
    annotated_return(db, definition)
        .is_some_and(|annotation| widens_enum_set(db, &env, annotation, types.return_ty))
}

/// The return annotation of `function`, defined by `definition`, as ty's return checks see it: none
/// when it is an imprecise operator result ([`has_imprecise_return`]), so the returns are checked
/// against the parent's result type.
pub(crate) fn return_annotation<'a>(
    db: &dyn Db,
    definition: Definition<'_>,
    function: &'a ast::StmtFunctionDef,
) -> Option<&'a ast::Expr> {
    function
        .returns
        .as_deref()
        .filter(|_| !has_imprecise_return(db, definition))
}

/// The return annotation of the mixin method `definition` that ty's override check accepts beside
/// `precise`, the result type of the Blender callback it stands in for: its own when it is
/// `precise` widened to `str`.
pub(super) fn accepted_mixin_return<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    definition: Definition<'db>,
    precise: Type<'db>,
) -> Option<Type<'db>> {
    annotated_return(db, definition)
        .filter(|annotation| widens_enum_set(db, env, *annotation, precise))
}

/// Whether `value`, returned where `expected` is a callback's result, is an imprecise operator result:
/// a `set[str]` where the result is an enum set.
pub(crate) fn is_imprecise_result<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value: Type<'db>,
    expected: Type<'db>,
) -> bool {
    is_str_set(db, env, value) && enum_set_item(db, env, expected).is_some()
}

/// Reports `blender-imprecise-operator-return` on a returned `set[str]`, `value`, that is no set
/// display: a set display gets its item types from the result type, so a display that is still a
/// `set[str]` holds a name that is not a result, which stays `invalid-explicit-return`. Returns whether
/// it was reported (the return is then accepted).
pub(crate) fn check_imprecise_result<'db>(
    context: &InferContext<'db, '_>,
    function: &ast::StmtFunctionDef,
    range: TextRange,
    value: Type<'db>,
    expected: Type<'db>,
) -> bool {
    let db = context.db();
    let env = context.program_environment();
    if !is_imprecise_result(db, env, value, expected) {
        return false;
    }
    let mut visitor = ReturnStatementVisitor::default();
    visitor.visit_body(&function.body);
    let is_display = visitor.returns.iter().any(|ret| {
        ret.value
            .as_deref()
            .is_some_and(|value| value.range() == range && is_set_display(value))
    });
    if is_display {
        return false;
    }
    if let Some(builder) = context.report_lint(&BLENDER_IMPRECISE_OPERATOR_RETURN, range) {
        builder.into_diagnostic(format_args!(
            "Returned `{}` is less precise than `{}`, the result names Blender accepts",
            value.display(db, env),
            expected.display(db, env),
        ));
    }
    true
}

/// A positional parameter of an override of a Blender callback annotated with a subtype of the
/// parent's type without `None`: `data: Armature` in `UIList.draw_item`, whose parent has
/// `data: bpy_struct`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct NarrowedParameter<'db> {
    /// The position of the parameter, `self` / `cls` being 0.
    pub(super) position: usize,
    /// The annotation's type.
    pub(super) annotation: Type<'db>,
    /// The parent's parameter type at that position.
    pub(super) parent: Type<'db>,
    /// The annotation.
    pub(super) range: TextRange,
}

/// The parameters of the method `definition`, an override of a Blender callback, annotated with a
/// subtype of the parent's parameter type without `None`, where the parent's type does not fit the
/// annotation. ty's override check compares them as they are annotated; an annotation that is no
/// such subtype is left to its `incompatible-method-override`.
#[salsa::tracked(
    returns(deref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn narrowed_parameters<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Box<[NarrowedParameter<'db>]> {
    let Some(types) = override_types(db, definition) else {
        return Box::default();
    };
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return Box::default();
    };
    let env = ProgramEnvironment::from_file(definition.program_file(db));
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let parameters = &function.node(&module).parameters;
    parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .enumerate()
        .filter_map(|(position, parameter)| {
            let annotation = parameter.parameter.annotation.as_deref()?;
            let parent = types.positional(position)?;
            let annotated = function_signature_expression_type(db, definition, annotation);
            (!parent.is_assignable_to(db, &env, annotated)
                && annotated.is_assignable_to(db, &env, without_none(db, parent)))
            .then(|| NarrowedParameter {
                position,
                annotation: annotated,
                parent,
                range: annotation.range(),
            })
        })
        .collect()
}

/// Reports the imprecise annotations of `function`, the `def` of the method `definition`: an
/// operator result annotated `set[str]` and callback parameters annotated with the subclass Blender
/// passes.
pub(super) fn check_method_annotations<'db>(
    context: &InferContext<'db, '_>,
    definition: Definition<'db>,
    function: &ast::StmtFunctionDef,
) {
    let db = context.db();
    let env = context.program_environment();
    if has_imprecise_return(db, definition)
        && let Some(returns) = function.returns.as_deref()
        && let Some(types) = override_types(db, definition)
    {
        report_imprecise_return(context, returns, &function.name, types.return_ty);
    }
    for parameter in narrowed_parameters(db, definition) {
        if let Some(builder) =
            context.report_lint(&BLENDER_IMPRECISE_CALLBACK_PARAMETER, parameter.range)
        {
            builder.into_diagnostic(format_args!(
                "Parameter annotated `{}` is narrower than `{}` of the method `{}` overrides; \
                 accepted, since only Blender calls it, with what the add-on gave it",
                parameter.annotation.display(db, env),
                parameter.parent.display(db, env),
                function.name,
            ));
        }
    }
}

fn report_imprecise_return(
    context: &InferContext<'_, '_>,
    returns: &ast::Expr,
    name: &ast::Identifier,
    precise: Type<'_>,
) {
    let db = context.db();
    let env = context.program_environment();
    if let Some(builder) = context.report_lint(&BLENDER_IMPRECISE_OPERATOR_RETURN, returns) {
        builder.into_diagnostic(format_args!(
            "Return annotation is less precise than `{}`, the result names Blender accepts; \
             `{name}` is checked as if annotated with it",
            precise.display(db, env),
        ));
    }
}

/// Reports `blender-imprecise-operator-return` on the methods of the mixins in `file` that a class
/// of `file` deriving from a Blender class gets a callback from, annotated with the callback's
/// enum-set result widened to `str`:
///
/// ```python
/// class ResultMixin:
///     def execute(self, context) -> set[str]:  # blender-imprecise-operator-return
///         return {"FINISHED"}
///
/// class MY_OT_run(ResultMixin, Operator): ...
/// ```
pub(super) fn check_mixin_returns(context: &InferContext, file: ProgramFile) {
    let db = context.db();
    let env = context.program_environment();
    let mut reported: Vec<Definition> = Vec::new();
    for class in &file_facts(db, file).classes {
        let class = class.class;
        let Some(root) = callback_root(db, class) else {
            continue;
        };
        for callback in callbacks_of(root) {
            let member = class.class_member_from_mro(
                db,
                env,
                callback.name,
                MemberLookupPolicy::default(),
                class.iter_mro(db, None),
            );
            let Some(Type::FunctionLiteral(function)) = member.place.ignore_possibly_undefined()
            else {
                continue;
            };
            if function.program_file(db) != file {
                continue;
            }
            let definition = function.definition(db);
            if reported.contains(&definition) || mixin_of(db, definition).is_none() {
                continue;
            }
            let Some(parent) = parent_callback_types(db, class, callback) else {
                continue;
            };
            if accepted_mixin_return(db, env, definition, parent.return_ty).is_none() {
                continue;
            }
            let DefinitionKind::Function(node) = definition.kind(db) else {
                continue;
            };
            let module = parsed_module(db, definition.python_file(db)).load(db);
            let node = node.node(&module);
            if let Some(returns) = node.returns.as_deref() {
                reported.push(definition);
                report_imprecise_return(context, returns, &node.name, parent.return_ty);
            }
        }
    }
}

/// Whether `ty`, a callable, is one of Blender's: a function, method or class defined in a Blender
/// stub module, or an operator of `bpy.ops`, built-in or the add-on's.
fn is_blender_callable<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    match ty {
        Type::FunctionLiteral(function) => is_blender_stub_file(db, function.program_file(db)),
        Type::BoundMethod(method) => method
            .function(db)
            .is_some_and(|function| is_blender_stub_file(db, function.program_file(db))),
        Type::ClassLiteral(class) => class
            .as_static()
            .is_some_and(|class| is_blender_stub_file(db, class.program_file(db))),
        _ => is_operator_call(db, ty),
    }
}

/// Whether `value` is an imprecise value for `expected`: a `str` where `expected` is an enum (a union
/// of string literals, `None` aside), or a `set[str]` where it is an enum flag set.
fn is_imprecise_enum_value<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value: Type<'db>,
    expected: Type<'db>,
) -> bool {
    let expected = without_none(db, expected.resolve_type_alias(db));
    if value == KnownClass::Str.to_instance(db, env) {
        return is_string_literals(db, expected);
    }
    is_str_set(db, env, value)
        && expected
            .class_specialization(db, env)
            .is_some_and(|(class, _)| class.is_known(db, KnownClass::Set))
        && enum_set_item(db, env, expected).is_some()
}

/// Whether ty's argument check accepts `argument`, passed to the Blender callable `callable` for a
/// parameter of type `expected`, as an imprecise enum value. [`check_enum_arguments`] reports it.
pub(crate) fn accepts_enum_argument<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    callable: Type<'db>,
    argument: Type<'db>,
    expected: Type<'db>,
) -> bool {
    is_imprecise_enum_value(db, env, argument, expected) && is_blender_callable(db, callable)
}

/// Reports `blender-imprecise-enum-value` on the arguments of `call` that ty's argument check
/// accepted as imprecise enum values ([`accepts_enum_argument`]).
pub(super) fn check_enum_arguments<'db>(
    context: &InferContext<'db, '_>,
    call: &ast::ExprCall,
    bindings: &Bindings<'db>,
    expression_type: &dyn Fn(&ast::Expr) -> Option<Type<'db>>,
) {
    let db = context.db();
    let env = context.program_environment();
    let arguments: Vec<&ast::Expr> = call
        .arguments
        .iter_source_order()
        .map(|argument| match argument {
            ast::ArgOrKeyword::Arg(expr) => expr,
            ast::ArgOrKeyword::Keyword(keyword) => &keyword.value,
        })
        .collect();
    let mut reported: Vec<TextRange> = Vec::new();
    for callable in bindings.iter_flat() {
        if !is_blender_callable(db, callable.signature_type) {
            continue;
        }
        for (_, binding) in callable.matching_overloads() {
            let matches = binding.argument_matches();
            // A bound method's receiver is matched too, before the arguments of the call.
            let offset = matches.len().saturating_sub(arguments.len());
            let parameters = binding.signature.parameters();
            for (argument, matched) in arguments.iter().zip(matches.iter().skip(offset)) {
                if reported.contains(&argument.range()) {
                    continue;
                }
                let Some(value) = expression_type(argument) else {
                    continue;
                };
                let Some(expected) = matched.parameters.iter().find_map(|parameter| {
                    parameters
                        .get(parameter.index)
                        .map(|parameter| parameter.annotated_type())
                        .filter(|expected| is_imprecise_enum_value(db, env, value, *expected))
                }) else {
                    continue;
                };
                reported.push(argument.range());
                if is_set_display(argument) {
                    report_set_display_argument(context, argument.range(), value, expected);
                } else {
                    report_enum_value(context, argument.range(), value);
                }
            }
        }
    }
}

/// Whether `value` is a set display (`{"A", "B"}`) or comprehension. A display gets its item types
/// from the enum set it is checked against, so one that is still a `set[str]` holds a name that is
/// not an item: it is never an imprecise value.
fn is_set_display(value: &ast::Expr) -> bool {
    matches!(value, ast::Expr::Set(_) | ast::Expr::SetComp(_))
}

/// Reports `invalid-argument-type` on a set display passed for an enum set that ty's argument check
/// accepted as a `set[str]` ([`accepts_enum_argument`] only sees the argument's type).
fn report_set_display_argument<'db>(
    context: &InferContext<'db, '_>,
    range: TextRange,
    value: Type<'db>,
    expected: Type<'db>,
) {
    let db = context.db();
    let env = context.program_environment();
    if let Some(builder) = context.report_lint(&INVALID_ARGUMENT_TYPE, range) {
        builder.into_diagnostic(format_args!(
            "Argument is incorrect: Expected `{}`, found `{}`: a name in the set is not an enum item",
            expected.display(db, env),
            value.display(db, env),
        ));
    }
}

fn report_enum_value(context: &InferContext<'_, '_>, range: TextRange, value: Type<'_>) {
    let db = context.db();
    let env = context.program_environment();
    if let Some(builder) = context.report_lint(&BLENDER_IMPRECISE_ENUM_VALUE, range) {
        builder.into_diagnostic(format_args!(
            "A `{}` value where Blender takes an enum item; Blender raises for a name that is not one",
            value.display(db, env),
        ));
    }
}

/// Whether `object_ty` is an instance of a Blender class: a class of a Blender stub module, or one
/// deriving from one. An intersection is one when one of its positive elements is, as the
/// properties of an operator given by idname are (`OperatorProperties & P`, `layout.operator(...)`).
fn is_blender_instance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    object_ty: Type<'db>,
) -> bool {
    let instance = match object_ty {
        Type::NominalInstance(instance) => instance,
        Type::Intersection(intersection) => {
            return intersection
                .positive(db)
                .iter()
                .any(|element| is_blender_instance(db, env, *element));
        }
        _ => return false,
    };
    let Some(class) = instance.class(db, env).class_literal(db).as_static() else {
        return false;
    };
    class.iter_mro(db, None).any(|base| match base {
        ClassBase::Class(base) => base
            .class_literal(db)
            .as_static()
            .is_some_and(|base| is_blender_stub_file(db, base.program_file(db))),
        _ => false,
    })
}

/// Whether the attribute write `target = value`, of a `value_ty` that is not assignable to the
/// attribute's `target_ty`, is an imprecise enum value written to an enum property of a Blender
/// instance, `object_ty`, and no set display ([`is_set_display`]). Reports
/// `blender-imprecise-enum-value` on it when `emit_diagnostics`.
pub(crate) fn check_enum_write<'db>(
    context: &InferContext<'db, '_>,
    object_ty: Type<'db>,
    target: &ast::ExprAttribute,
    value: &ast::Expr,
    value_ty: Type<'db>,
    target_ty: Type<'db>,
    emit_diagnostics: bool,
) -> bool {
    let db = context.db();
    let env = context.program_environment();
    if is_set_display(value)
        || !is_imprecise_enum_value(db, env, value_ty, target_ty)
        || !is_blender_instance(db, env, object_ty)
    {
        return false;
    }
    if emit_diagnostics {
        report_enum_value(context, target.range(), value_ty);
    }
    true
}

/// The class whose body defines the method `definition`, when it is a mixin: a class that derives
/// from no `bpy.types` class.
fn mixin_of<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Option<StaticClassLiteral<'db>> {
    defining_class(db, definition).filter(|class| callback_root(db, *class).is_none())
}
