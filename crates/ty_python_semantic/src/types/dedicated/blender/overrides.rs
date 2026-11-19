//! Overrides of Blender callbacks: what Blender refuses when it registers a class, and what ty's own
//! override checks compare a callback with.
//!
//! Blender looks up each callback of a class's registrable root on the class when it registers it
//! (the table in `ty_blender::callbacks`), and refuses the class when the callback is a plain method
//! where it needs a classmethod or the reverse, or has the wrong number of positional parameters.
//! It calls a callback with positional arguments only, as many as the callback has parameters:
//!
//! ```python
//! class MY_OT_run(Operator):
//!     def execute(self, ctx):  # fine: Blender passes `context` by position
//!         ...
//!
//!     def invoke(self, context):  # error: [blender-invalid-override] Blender passes an event too
//!         ...
//! ```
//!
//! - [`check_overrides`] reports `blender-invalid-override` on each `def` of a callback that Blender
//!   refuses, in a module-level class deriving from a registrable `bpy.types` class, registered or
//!   not: a registered subclass inherits the `def`.
//! - [`override_contract`] is what ty's `incompatible-method-override` compares such an override with:
//!   nothing where `blender-invalid-override` reports, otherwise the parent's signature with
//!   positional-only parameters, so a renamed parameter or a short `UIList.draw_item` is accepted
//!   while a parameter or return type that does not fit still is not.
//! - [`override_types`] is what an override takes from its parent where it has no annotation: the
//!   parent's parameter types by position and its return type. [`apply_override_types`] puts them
//!   in the override's signature, for callers, and [`override_parameter_type`] gives them to the
//!   parameters in its body (an operator's `description` gets its own properties as `properties`).
//!   [`override_return_type`] is what its `return` values are inferred with and checked against
//!   when it has no return annotation.

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::name::Name;
use ruff_text_size::Ranged;
use ty_blender::{
    Callback, CallbackShape, MethodKind, callback_of, is_callback_name, is_callback_root,
};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::FileScopeId;
use ty_python_core::{ProgramFile, semantic_index};

use crate::types::class_base::ClassBase;
use crate::types::context::InferContext;
use crate::types::infer::{function_known_decorators, nearest_enclosing_class};
use crate::types::signatures::CallableSignature;
use crate::types::{
    ClassType, KnownClass, LiteralValueType, MemberLookupPolicy, Parameter, Parameters, Signature,
    StaticClassLiteral, Type, UnionType,
};
use crate::{Db, ProgramEnvironment};

use super::imprecise::{
    NarrowedParameter, accepted_mixin_return, check_method_annotations, check_mixin_returns,
    has_imprecise_return, narrowed_parameters,
};
use super::index::file_facts;
use super::lints::BLENDER_INVALID_OVERRIDE;
use super::list_sources::{list_data_type, list_item_type};
use super::operator_properties::class_properties_type;
use super::symbols::{bpy_types_bases, bpy_types_class_name, is_bpy_types_class};
use super::vector::module_class;

/// The root whose callbacks `class` has: the first `bpy.types` class in its method resolution
/// order that has callbacks (`Node` for a `ShaderNode` subclass). `None` for a `bpy.types` class
/// itself.
pub(super) fn callback_root<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<&'db str> {
    if is_bpy_types_class(db, class) {
        return None;
    }
    bpy_types_bases(db, class).find(|base| is_callback_root(base))
}

/// The name and shape of the method `definition`: a `def` in the body of a module-level class whose
/// decorators are `classmethod`, `staticmethod` or none ([`method_kind`]). Any other decorator, and
/// any other binding (`poll = shared_poll`), gives `None`.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn method_shape<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<(Name, CallbackShape)> {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let file = definition.program_file(db);
    let index = semantic_index(db, file);
    let scope = definition.file_scope(db);
    if index.scope(scope).node().as_class().is_none()
        || index.parent_scope_id(scope) != Some(FileScopeId::global())
    {
        return None;
    }
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let function = function.node(&module);
    let kind = method_kind(db, definition, function)?;
    let parameters = &function.parameters;
    Some((
        function.name.id.clone(),
        CallbackShape {
            kind,
            positional: parameters.posonlyargs.len() + parameters.args.len(),
        },
    ))
}

/// The kind of the method `definition`, whose `def` is `function`, from its decorators: `None` when
/// one of them is neither `classmethod` nor `staticmethod`. Blender then gets whatever that
/// decorator returns, which may take other parameters than the `def`, even where its annotations say
/// it gives back the function itself.
fn method_kind<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    function: &ast::StmtFunctionDef,
) -> Option<MethodKind> {
    let decorators = function_known_decorators(db, definition);
    let mut kind = MethodKind::Function;
    // The outermost decorator, listed first, decides.
    for decorator in function.decorator_list.iter().rev() {
        let Some(Type::ClassLiteral(class)) = decorators.expression_type(&decorator.expression)
        else {
            return None;
        };
        kind = match class.known(db)? {
            KnownClass::Classmethod => MethodKind::ClassMethod,
            KnownClass::Staticmethod => MethodKind::StaticMethod,
            _ => return None,
        };
    }
    Some(kind)
}

/// The class whose body defines `definition`.
pub(super) fn defining_class<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<StaticClassLiteral<'db>> {
    let file = definition.program_file(db);
    nearest_enclosing_class(db, semantic_index(db, file), definition.scope(db))
}

/// Why Blender refuses the method `definition` when it registers its class (or a subclass), or
/// `None` when it accepts it or the method is no Blender callback.
///
/// Both [`check_overrides`] and [`override_contract`] read it, so `blender-invalid-override` and
/// ty's `incompatible-method-override` never report the same `def`.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn callback_problem<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<String> {
    let (name, shape) = method_shape(db, definition).as_ref()?;
    let root = callback_root(db, defining_class(db, definition)?)?;
    callback_of(root, name.as_str())?.problem(*shape)
}

/// Reports `blender-invalid-override` on the callbacks that Blender refuses, in the module-level
/// classes of `file` that `register_class` accepts.
pub(super) fn check_overrides(context: &InferContext, file: ProgramFile) {
    let db = context.db();
    let module = parsed_module(db, file.python_file(db)).load(db);
    let index = semantic_index(db, file);
    for class in &file_facts(db, file).classes {
        let DefinitionKind::Class(class_node) = class.class.definition(db).kind(db) else {
            continue;
        };
        for statement in &class_node.node(&module).body {
            let ast::Stmt::FunctionDef(function) = statement else {
                continue;
            };
            let Some(definition) = index.try_definition(function) else {
                continue;
            };
            if let Some(problem) = callback_problem(db, definition)
                && let Some(builder) =
                    context.report_lint(&BLENDER_INVALID_OVERRIDE, function.name.range())
            {
                builder.into_diagnostic(problem.clone());
            }
            check_method_annotations(context, definition, function);
        }
    }
    check_mixin_returns(context, file);
}

/// What ty's override checks compare the method `name` of `class`, defined by `definition`, with,
/// given `contract`, the parent's callable bound to `class`.
///
/// - `contract` itself when `name` is no Blender callback of `class`;
/// - `None` when `blender-invalid-override` reports `definition`: Blender's verdict replaces ty's;
/// - otherwise `contract` with every positional parameter made positional-only, since Blender
///   passes arguments by position. For `UIList.draw_item`, which Blender calls with as many
///   arguments as it has parameters, the parameters beyond the override's are dropped. Parameter
///   and return types are kept, so an annotation that does not fit is still reported.
///
/// `definition` is in the body of `class`, or of a mixin that `class` inherits it from.
pub(crate) fn override_contract<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: ClassType<'db>,
    name: &str,
    definition: Definition<'db>,
    contract: Type<'db>,
) -> Option<Type<'db>> {
    let Some(callback) = class
        .class_literal(db)
        .as_static()
        .and_then(|class| callback_root(db, class))
        .and_then(|root| callback_of(root, name))
    else {
        return Some(contract);
    };
    if callback_problem(db, definition).is_some() {
        return None;
    }
    // A bound contract has no `self` / `cls`.
    let kept = (callback.positional.start() != callback.positional.end())
        .then(|| method_shape(db, definition).as_ref())
        .flatten()
        .map(|(_, shape)| shape.positional.saturating_sub(1));
    let accepted = AcceptedAnnotations {
        narrowed: narrowed_parameters(db, definition),
        definition,
    };
    Some(with_positional_only_parameters(
        db, env, contract, kept, &accepted,
    ))
}

/// The annotations of an override, or of a mixin method standing in for one, that ty's override
/// check accepts although they do not fit the parent's signature: the Imprecise annotations
/// (`super::imprecise`).
struct AcceptedAnnotations<'a, 'db> {
    /// Parameters annotated with a subtype of the parent's type.
    narrowed: &'a [NarrowedParameter<'db>],
    /// The method, whose return annotation may be the parent's enum-set result widened to `str`.
    definition: Definition<'db>,
}

/// `contract` with its positional parameters made positional-only, keeping the first `kept` of them
/// when that is given.
fn with_positional_only_parameters<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    contract: Type<'db>,
    kept: Option<usize>,
    accepted: &AcceptedAnnotations<'_, 'db>,
) -> Type<'db> {
    match contract {
        Type::Union(union) => union.map(db, env, |element| {
            with_positional_only_parameters(db, env, *element, kept, accepted)
        }),
        Type::Callable(callable) => {
            let overloads = callable
                .signatures(db)
                .iter()
                .map(|signature| positional_only_signature(db, env, signature, kept, accepted));
            Type::Callable(
                callable.with_signatures(db, CallableSignature::from_overloads(overloads)),
            )
        }
        _ => contract,
    }
}

fn positional_only_signature<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    signature: &Signature<'db>,
    kept: Option<usize>,
    accepted: &AcceptedAnnotations<'_, 'db>,
) -> Signature<'db> {
    if !signature.parameters().is_standard() {
        return signature.clone();
    }
    let widened = widen_array_outputs(db, env, signature.return_ty);
    let return_ty = match accepted_mixin_return(db, env, accepted.definition, signature.return_ty) {
        Some(annotation) => UnionType::from_two_elements(db, env, widened, annotation),
        None => widened,
    };
    let mut positional = 0;
    let parameters = signature.parameters().iter().filter_map(|parameter| {
        if !parameter.is_positional() {
            return Some(parameter.clone());
        }
        positional += 1;
        if kept.is_some_and(|kept| positional > kept) {
            return None;
        }
        // A bound contract has no `self` / `cls`, so its first positional parameter is the
        // override's second.
        let annotated_type = accepted
            .narrowed
            .iter()
            .find(|narrowed| narrowed.position == positional)
            .map_or(parameter.annotated_type(), |narrowed| narrowed.annotation);
        Some(
            Parameter::positional_only(parameter.name().cloned())
                .with_annotated_type(annotated_type)
                .with_optional_default_type(parameter.default_type(db))
                .with_definition(parameter.definition()),
        )
    });
    Signature::new_generic(
        signature.generic_context,
        Parameters::standard(parameters.collect::<Vec<_>>()),
        return_ty,
    )
    .with_definition(signature.definition)
}

/// `ty` with each `bpy_prop_array[T]` in it widened to `bpy_prop_array[T] | Sequence[T]`, also
/// inside a fixed-length tuple.
///
/// Blender accepts any sequence of the right length where a callback returns an array, so this
/// is what an override's return values and return annotation are checked with. The stubs'
/// `bpy_prop_array` is no `Sequence`, so it is kept beside it. Callers of the override still see
/// the parent's array type:
///
/// ```python
/// class MY_Socket(NodeSocket):
///     def draw_color(self, context, node):  # `-> bpy_prop_array[float]` for callers
///         return (1.0, 1.0, 1.0, 1.0)  # fine: a `Sequence[float]`
/// ```
///
/// A callback with several outputs returns them as a tuple, each output widened on its own:
/// `UIList.filter_items` returns `(flags, order)`, two lists of ints.
///
/// An enum-set output, `set[Literal[...]]`, is widened to `collections.abc.Set[Literal[...]]`:
/// Blender reads any set of the enum's names, so an override may return or be annotated with a
/// set of fewer of them (`-> set[Literal["FINISHED"]]`) or a `frozenset`, which the invariant
/// `set` would reject. A name that is not one of the enum's stays an error.
fn widen_array_outputs<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Type<'db> {
    if let Type::Union(union) = ty {
        return union.map(db, env, |element| widen_array_outputs(db, env, *element));
    }
    if let Some(spec) = ty.exact_tuple_instance_spec(db)
        && let Some(tuple) = spec.as_fixed_length()
    {
        let elements: Vec<Type<'db>> = tuple
            .all_elements()
            .iter()
            .map(|element| widen_array_outputs(db, env, *element))
            .collect();
        return Type::heterogeneous_tuple(db, env, elements);
    }
    if let Some((class, specialization)) = ty.class_specialization(db, env)
        && class.is_known(db, KnownClass::Set)
        && let [item] = specialization.types(db)
        && is_string_literals(db, *item)
        && let Some(abstract_set) = module_class(db, env, "typing", "AbstractSet")
    {
        let abstract_set = abstract_set.apply_specialization(db, |generic_context| {
            generic_context.specialize(db, vec![*item])
        });
        return Type::instance(db, env, abstract_set);
    }
    if let Some((class, specialization)) = ty.class_specialization(db, env)
        && bpy_types_class_name(db, class) == Some("bpy_prop_array")
        && let [item] = specialization.types(db)
    {
        let sequence = KnownClass::Sequence.to_specialized_instance(db, env, &[*item]);
        return UnionType::from_two_elements(db, env, ty, sequence);
    }
    ty
}

/// Whether `ty` is a string literal or a union of them: an enum value, or the item type of an enum
/// set.
pub(super) fn is_string_literals(db: &dyn Db, ty: Type<'_>) -> bool {
    let is_string = |ty: Type<'_>| {
        ty.as_literal_value()
            .and_then(LiteralValueType::as_string)
            .is_some()
    };
    match ty {
        Type::Union(union) => union.elements(db).iter().all(|element| is_string(*element)),
        ty => is_string(ty),
    }
}

/// What an override of a Blender callback takes from its parent where it has no annotation.
///
/// Blender passes a callback's arguments by position and with the parent's types, so an
/// unannotated parameter takes the type of the parent's parameter at the same position, whatever
/// its name, and an unannotated return the parent's return type:
///
/// ```python
/// class MY_OT_run(Operator):
///     def invoke(self, ctx, event):  # `ctx: Context`, `event: Event`, `-> set[Literal[...]]`
///         ...
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct OverrideTypes<'db> {
    /// The types of the parent's positional parameters after `self` / `cls`, in order.
    positional: Box<[Type<'db>]>,
    /// The parent's return type.
    pub(crate) return_ty: Type<'db>,
    /// Whether Blender converts the callback's result with `bool()` (`poll`, `check`, ...), so it
    /// accepts any return value.
    bool_result: bool,
    /// The position of the parameter Blender passes the operator's own properties to
    /// ([`ty_blender::Callback::own_properties_position`]).
    own_properties_position: Option<usize>,
    /// The position of the parameter taken as not `None` in the body
    /// ([`ty_blender::Callback::assumed_item_position`]).
    pub(super) assumed_item_position: Option<usize>,
    /// The position of a list's `data` ([`ty_blender::Callback::list_data_position`]).
    list_data_position: Option<usize>,
}

impl<'db> OverrideTypes<'db> {
    /// The type of the positional parameter at `index`, `self` / `cls` being 0, or `None` when the
    /// parent has no parameter there.
    pub(crate) fn positional(&self, index: usize) -> Option<Type<'db>> {
        self.positional.get(index.checked_sub(1)?).copied()
    }
}

/// What the method `definition` takes from the method it overrides, when it is a Blender callback
/// of its class's registrable root.
///
/// The parent is the first class after the method's class in its method resolution order that
/// defines the method and is a `bpy.types` class or derives from one. A plain mixin is skipped: it
/// is no Blender class, so it has no types of Blender's to give, and its own methods get no answer
/// either. An overloaded parent, a parent that is no function, a `staticmethod` override and one
/// with any other decorator but `classmethod` ([`method_kind`]) give none.
///
/// The parent's types are taken as they are, `X | None` included, with the parent's `Self` bound
/// to the overriding class. Registration plays no part: an unregistered base is typed too.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
pub(crate) fn override_types<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<OverrideTypes<'db>> {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let function = function.node(&module);
    let name = function.name.id.clone();
    // `raw_signature` asks for every method: rule most of them out by their name alone, before
    // any method resolution order is looked at.
    if !is_callback_name(&name) {
        return None;
    }
    let class = defining_class(db, definition)?;
    let callback = callback_of(callback_root(db, class)?, &name)?;
    if !matches!(
        method_kind(db, definition, function),
        Some(MethodKind::Function | MethodKind::ClassMethod)
    ) {
        return None;
    }

    parent_callback_types(db, class, callback)
}

/// What a method `callback` of `class` takes from the callback it overrides: the types of the first
/// class after `class` in its method resolution order that defines it and is a `bpy.types` class or
/// derives from one ([`override_types`]).
pub(super) fn parent_callback_types<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    callback: &Callback,
) -> Option<OverrideTypes<'db>> {
    let env = ProgramEnvironment::from_file(class.program_file(db));
    for base in class.iter_mro(db, None).skip(1) {
        let ClassBase::Class(base_class) = base else {
            continue;
        };
        let Some(base_literal) = base_class.class_literal(db).as_static() else {
            continue;
        };
        if bpy_types_bases(db, base_literal).next().is_none() {
            continue;
        }
        let member = class.class_member_from_mro(
            db,
            &env,
            callback.name,
            MemberLookupPolicy::default(),
            std::iter::once(base),
        );
        let Some(member) = member.place.ignore_possibly_undefined() else {
            continue;
        };
        let Type::FunctionLiteral(parent) = member else {
            return None;
        };
        let [signature] = parent.signature(db).overloads.as_slice() else {
            return None;
        };
        let instance = Type::instance(db, &env, class.default_specialization(db));
        let bound = signature.bind_self(db, &env, Some(instance));
        if !bound.parameters().is_standard() {
            return None;
        }
        return Some(OverrideTypes {
            positional: bound
                .parameters()
                .iter()
                .filter(|parameter| parameter.is_positional())
                .map(Parameter::annotated_type)
                .collect(),
            return_ty: bound.return_ty,
            bool_result: callback.bool_result,
            own_properties_position: callback.own_properties_position(),
            assumed_item_position: callback.assumed_item_position(),
            list_data_position: callback.list_data_position(),
        });
    }
    None
}

/// Gives the unannotated parameters and return of `signature`, the raw signature of the method
/// `definition`, the types of the Blender callback it overrides ([`override_types`]), so callers,
/// signature help and ty's override checks see them. An unannotated keyword-only parameter with a
/// default, which Blender never passes, takes its default's promoted type (`verbose=False` gives
/// `bool`); `*args` and `**kwargs` keep theirs.
pub(crate) fn apply_override_types<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    definition: Definition<'db>,
    signature: &mut Signature<'db>,
) {
    let Some(types) = override_types(db, definition) else {
        return;
    };
    if !signature.parameters().is_standard() {
        return;
    }
    let mut position = 0;
    let parameters = signature
        .parameters()
        .iter()
        .map(|parameter| {
            let ty = if parameter.is_positional() {
                position += 1;
                types.positional(position - 1)
            } else if parameter.is_keyword_only() {
                parameter
                    .default_type(db)
                    .map(|default| default.promote(db, env))
            } else {
                None
            };
            match ty {
                Some(ty) if parameter.inferred_annotation => {
                    parameter.clone().with_annotated_type(ty)
                }
                _ => parameter.clone(),
            }
        })
        .collect::<Vec<_>>();
    // `-> set[str]` on `execute` is typed as the parent's result (`super::imprecise`).
    let return_annotated = match definition.kind(db) {
        DefinitionKind::Function(function) => {
            let module = parsed_module(db, definition.python_file(db)).load(db);
            function.node(&module).returns.is_some() && !has_imprecise_return(db, definition)
        }
        _ => true,
    };
    let return_ty = if return_annotated {
        signature.return_ty
    } else {
        types.return_ty
    };
    *signature = Signature::new_generic(
        signature.generic_context,
        Parameters::standard(parameters),
        return_ty,
    )
    .with_definition(signature.definition);
}

/// The type of the unannotated parameter `parameter` in the body of a method that overrides a
/// Blender callback ([`override_types`]): the parent's parameter type at its position, or for a
/// keyword-only parameter the promoted type of its default `default`. `None` when the method
/// overrides no callback or the parent has no parameter at that position.
pub(crate) fn override_parameter_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    parameter: Definition<'db>,
    default: Option<Type<'db>>,
) -> Option<Type<'db>> {
    let file = parameter.program_file(db);
    let index = semantic_index(db, file);
    let function = index.scope(parameter.file_scope(db)).node().as_function()?;
    let definition = index.expect_single_definition(function);
    let types = override_types(db, definition).as_ref()?;
    let module = parsed_module(db, parameter.python_file(db)).load(db);
    let name_range = parameter.kind(db).target_range(&module);
    let parameters = &function.node(&module).parameters;
    if let Some(position) = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .position(|candidate| candidate.parameter.name.range() == name_range)
    {
        // Blender passes an operator's own properties to `description(cls, context, properties)`,
        // not the `OperatorProperties` of the stub. Only the body sees them: callers and the
        // override check keep the parent's type.
        if types.own_properties_position == Some(position)
            && let Some(class) = defining_class(db, definition)
            && let Some(properties) = class_properties_type(db, env, file, class)
        {
            return Some(properties);
        }
        // `UIList.draw_item`'s `item`: an Assumption, reported at its reads
        // ([`super::assumptions`]), typed by the list's source where the add-on shows it
        // ([`super::list_sources`]). Callers and the override check keep the parent's type.
        if types.assumed_item_position == Some(position) {
            return types
                .positional(position)
                .map(|ty| assumed_item_type(db, definition, super::member::without_none(db, ty)));
        }
        // `UIList.draw_item`'s and `filter_items`' `data`: the `dataptr` the add-on shows the list
        // with ([`super::list_sources`]).
        if types.list_data_position == Some(position) {
            return types
                .positional(position)
                .map(|ty| match defining_class(db, definition) {
                    Some(class) => list_data_type(db, class, ty),
                    None => ty,
                });
        }
        return types.positional(position);
    }
    if parameters
        .kwonlyargs
        .iter()
        .any(|candidate| candidate.parameter.name.range() == name_range)
    {
        return default.map(|default| default.promote(db, env));
    }
    None
}

/// The type of `UIList.draw_item`'s `item` in the method `definition`, whose parent gives it
/// `parent` (`None` excluded): the element type of the list's source, else `parent`.
pub(super) fn assumed_item_type<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    parent: Type<'db>,
) -> Type<'db> {
    match defining_class(db, definition) {
        Some(class) => list_item_type(db, class, parent),
        None => parent,
    }
}

/// What the return values of the method `definition` must fit when it overrides a Blender callback
/// without a return annotation ([`override_types`]): the parent's return type, an array return
/// also accepting a sequence. `None` when the method overrides no callback, and for a callback
/// whose result Blender converts with `bool()` (`poll`, `check`, ...), which accepts any value.
///
/// It gives the values of `return` statements their type context, so `{"FINISHED"}` in `execute`
/// is a set of literals, and `check_blender_override_returns` checks the returns against it.
pub(crate) fn override_return_type<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<Type<'db>> {
    let types = override_types(db, definition).as_ref()?;
    if types.bool_result {
        return None;
    }
    let env = ProgramEnvironment::from_file(definition.program_file(db));
    Some(widen_array_outputs(db, &env, types.return_ty))
}
