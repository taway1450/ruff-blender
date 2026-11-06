//! Operator properties: what an operator idname given as a string stands for.
//!
//! Blender's functions that take an operator idname (`layout.operator("my_addon.run")`,
//! `keymap_items.new("my_addon.run", ...)`, `Macro.define("MY_ADDON_OT_run")`, ...) return or hold
//! that operator's properties. The stubs can only say `OperatorProperties`, so the result of such a
//! call (a *sink*) is refined here from the idname ([`on_call`]):
//!
//! - the idname is the argument's literal string, or the `bl_idname` of the class `X` in
//!   `X.bl_idname`, which ty widens to `str`;
//! - it is looked up as Blender does: converted to its `CATEGORY_OT_name` form, then found among
//!   the operators of the file's add-on, then among Blender's own operators, read from the
//!   `bpy.ops` stubs;
//! - the properties are `OperatorProperties & P`, where `P` is a synthesised protocol with one
//!   read-only member per property and a hidden identity member, `__blender_operator__`, which
//!   holds the idname. The nominal half keeps `is_property_set()` and assignability to
//!   `OperatorProperties`; the protocol half names the operator. Reads ([`member`]) take `P`'s
//!   members, and never fake-bpy-module's `__getattr__`, so unknown names stay errors. Writes
//!   ([`write_type`]) look the operator up again through the identity to take the property's
//!   write type.
//! - a key-map item is `KeyMapItem & K` and a macro step `OperatorMacro & K`, where `K` holds
//!   `properties` and the identity.
//!
//! An idname that neither the add-on nor Blender has is `blender-unknown-operator`, unless the
//! add-on's `register()` could not be followed to its end (the operator may be registered where
//! the walk could not look) or the file belongs to no add-on.

use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_blender::OperatorIdname;
use ty_module_resolver::{ModuleName, file_to_module, resolve_module_confident};
use ty_python_core::scope::NodeWithScopeRef;
use ty_python_core::{ProgramFile, global_scope, place_table, semantic_index};

use crate::lint::LintMetadata;
use crate::place::{Place, PlaceAndQualifiers, imported_symbol};
use crate::semantic_model::ExpectedStringLiteralCompletion;
use crate::types::call::Bindings;
use crate::types::context::InferContext;
use crate::types::infer::{infer_complete_scope_types, nearest_enclosing_class};
use crate::types::list_members::Member;
use crate::types::{
    IntersectionType, KnownClass, LiteralValueType, MemberLookupPolicy, StaticClassLiteral,
    SubclassOfInner, Type, TypeVarBoundOrConstraints, UnionType,
};
use crate::{Db, ProgramEnvironment};

use super::index::addon_in_program;
use super::lints::{BLENDER_INVALID_OPERATOR_PROPERTY, BLENDER_UNKNOWN_OPERATOR};
use super::operators::{
    AddonOperator, addon_operators, is_macro_class, is_operator_class, operator_properties,
};
use super::properties::PropertyKind;
use super::registration_checks::{class_string, operator_idname};
use super::symbols::{blender_module_name, bpy_types_bases};
use super::vector::module_class;

/// The hidden member of a properties type that names its operator.
const IDENTITY: &str = "__blender_operator__";

/// How deep macros in macros are followed.
const MAX_MACRO_DEPTH: u32 = 4;

/// What a sink gives for a known idname.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SinkResult {
    /// The properties, never `None`.
    Properties,
    /// The stub's return type with `K` (a key-map item, a macro step).
    Holder,
    /// The stub's return type (`None`).
    Unchanged,
}

/// A Blender function that takes an operator idname.
#[derive(Copy, Clone, Debug)]
struct Sink {
    /// The `bpy.types` class that defines it.
    class: &'static str,
    method: &'static str,
    /// The name of its idname parameter (always the first).
    parameter: &'static str,
    result: SinkResult,
    /// Whether its second argument, `property`, names an enum property of the operator.
    enum_property: bool,
}

const SINKS: [Sink; 9] = [
    Sink::new("UILayout", "operator", SinkResult::Properties),
    Sink::new("UILayout", "operator_menu_hold", SinkResult::Properties),
    Sink::new("UILayout", "operator_menu_enum", SinkResult::Properties).with_enum_property(),
    Sink::new("UILayout", "operator_enum", SinkResult::Unchanged).with_enum_property(),
    Sink::new(
        "WindowManager",
        "operator_properties_last",
        SinkResult::Properties,
    ),
    Sink::new("Gizmo", "target_set_operator", SinkResult::Properties),
    Sink::new(
        "WorkSpaceTool",
        "operator_properties",
        SinkResult::Properties,
    ),
    Sink {
        parameter: "idname",
        ..Sink::new("KeyMapItems", "new", SinkResult::Holder)
    },
    Sink::new("Macro", "define", SinkResult::Holder),
];

impl Sink {
    const fn new(class: &'static str, method: &'static str, result: SinkResult) -> Self {
        Sink {
            class,
            method,
            parameter: "operator",
            result,
            enum_property: false,
        }
    }

    const fn with_enum_property(self) -> Self {
        Sink {
            enum_property: true,
            ..self
        }
    }
}

/// The sink `callable` is: a bound method defined in `bpy.types` with a name of the table, on a
/// receiver whose class derives from the table's class.
fn sink_of<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    callable: Type<'db>,
) -> Option<Sink> {
    let Type::BoundMethod(method) = callable else {
        return None;
    };
    let function = method.function(db)?;
    if blender_module_name(db, function.program_file(db)) != Some("bpy.types") {
        return None;
    }
    let name = function.name(db).as_str();
    let receiver = receiver_class(db, env, method.self_instance(db))?;
    SINKS.into_iter().find(|sink| {
        sink.method == name && bpy_types_bases(db, receiver).any(|base| base == sink.class)
    })
}

/// The class of a method's receiver, an instance or a class object.
fn receiver_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    receiver: Type<'db>,
) -> Option<StaticClassLiteral<'db>> {
    match receiver {
        Type::NominalInstance(instance) => instance.class(db, env).class_literal(db).as_static(),
        Type::ClassLiteral(class) => class.as_static(),
        Type::SubclassOf(subclass_of) => match subclass_of.subclass_of() {
            SubclassOfInner::Class(class) => class.class_literal(db).as_static(),
            _ => None,
        },
        Type::TypeVar(typevar) => match typevar.typevar(db).bound_or_constraints(db, env)? {
            TypeVarBoundOrConstraints::UpperBound(bound) => receiver_class(db, env, bound),
            TypeVarBoundOrConstraints::Constraints(_) => None,
        },
        _ => None,
    }
}

/// The string a type stands for: a string literal, or a union of dynamic types and one string
/// literal.
fn literal_string(db: &dyn Db, ty: Type<'_>) -> Option<String> {
    let string = |ty: Type<'_>| {
        ty.as_literal_value()
            .and_then(LiteralValueType::as_string)
            .map(|string| string.value(db).to_string())
    };
    match ty {
        Type::Union(union) => {
            let mut strings = union
                .elements(db)
                .iter()
                .filter_map(|element| string(*element));
            let first = strings.next()?;
            (strings.next().is_none()
                && union
                    .elements(db)
                    .iter()
                    .all(|element| element.is_dynamic() || string(*element).is_some()))
            .then_some(first)
        }
        ty => string(ty),
    }
}

/// The idname an argument gives: its literal string, or for `X.bl_idname` the idname of the
/// operator class `X` (or of `X`'s class).
fn argument_idname<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    argument_type: Type<'db>,
    argument: Option<&Expr>,
    expression_type: &dyn Fn(&Expr) -> Option<Type<'db>>,
) -> Option<OperatorIdname> {
    if let Some(string) = literal_string(db, argument_type) {
        return OperatorIdname::from_lookup(&string);
    }
    let Some(Expr::Attribute(attribute)) = argument else {
        return None;
    };
    if attribute.attr.as_str() != "bl_idname" {
        return None;
    }
    let class = receiver_class(db, env, expression_type(&attribute.value)?)?;
    if !is_operator_class(db, class) {
        return None;
    }
    operator_idname(db, class)
}

/// A property of an operator, of an add-on or of Blender.
#[derive(Clone, Debug)]
pub(super) struct FoundProperty<'db> {
    pub(super) name: Name,
    /// What reading it gives.
    value: Type<'db>,
    /// What a write accepts, `None` when it is read-only.
    pub(super) write: Option<Type<'db>>,
    /// Whether it is an enum property, `None` when that is unknown (a built-in `str` property may
    /// be a dynamic enum).
    is_enum: Option<bool>,
}

/// An operator an idname names, with its properties.
#[derive(Clone, Debug)]
pub(super) struct FoundOperator<'db> {
    /// Its `CATEGORY_OT_name` form, what the identity member holds.
    pub(super) bl_form: String,
    pub(super) properties: Vec<FoundProperty<'db>>,
    pub(super) origin: OperatorOrigin<'db>,
}

/// Where an operator comes from.
#[derive(Clone, Debug)]
pub(super) enum OperatorOrigin<'db> {
    Addon(StaticClassLiteral<'db>),
    /// A built-in operator, with the type of `bpy.ops.<category>.<name>`.
    Builtin(Type<'db>),
}

/// The operator `idname` names in `file`, as Blender's lookup finds it: the add-on's operators
/// first, then Blender's.
pub(super) fn find_operator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    idname: &OperatorIdname,
) -> Option<FoundOperator<'db>> {
    find_operator_at(db, env, file, idname, 0)
}

fn find_operator_at<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    idname: &OperatorIdname,
    depth: u32,
) -> Option<FoundOperator<'db>> {
    if let Some(key) = addon_in_program(db, file)
        && let Some(operator) = addon_operators(db, key).get(&idname.category, &idname.name)
    {
        return Some(addon_operator(db, env, file, operator, depth));
    }
    builtin_operator(db, env, idname)
}

/// An operator of the add-on, with its properties, or a macro with one property per step.
fn addon_operator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    operator: &AddonOperator<'db>,
    depth: u32,
) -> FoundOperator<'db> {
    let class = operator.class;
    let bl_form = operator.idname().bl_form();
    if !is_macro_class(db, class) {
        return class_operator(db, env, class, bl_form);
    }
    let properties = operator
        .steps
        .iter()
        .filter(|_| depth < MAX_MACRO_DEPTH)
        .filter_map(|step| {
            let idname = OperatorIdname::from_lookup(step)?;
            let step_operator = find_operator_at(db, env, file, &idname, depth + 1)?;
            Some(FoundProperty {
                name: step.clone(),
                value: properties_type(db, env, &step_operator)?,
                write: None,
                is_enum: Some(false),
            })
        })
        .collect();
    FoundOperator {
        bl_form,
        properties,
        origin: OperatorOrigin::Addon(class),
    }
}

/// An operator class with its own properties.
fn class_operator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: StaticClassLiteral<'db>,
    bl_form: String,
) -> FoundOperator<'db> {
    let properties = operator_properties(db, class)
        .iter()
        .map(|property| {
            let is_enum = match &property.value {
                super::evaluate::PropertyValue::Declarations(declarations) => Some(
                    declarations
                        .iter()
                        .all(|declaration| declaration.kind == PropertyKind::Enum),
                ),
                super::evaluate::PropertyValue::Opaque => None,
            };
            FoundProperty {
                name: property.name.clone(),
                value: property.value.value_type(db, env),
                write: (!property.value.is_read_only(db))
                    .then(|| property.value.write_type(db, env)),
                is_enum,
            }
        })
        .collect();
    FoundOperator {
        bl_form,
        properties,
        origin: OperatorOrigin::Addon(class),
    }
}

/// One of Blender's operators, read from the `bpy.ops` stubs: the keyword-only parameters of
/// `bpy.ops.<category>.<name>` (a function in Blender's own stubs, the `__new__` of a class in
/// fake-bpy-module's). A parameter's annotation without `None` is what a write accepts, and with a
/// vector's sequence arm dropped (`Vector | Sequence[float]`) what a read gives.
pub(super) fn builtin_operator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    idname: &OperatorIdname,
) -> Option<FoundOperator<'db>> {
    let module_name = ModuleName::new(&format!("bpy.ops.{}", idname.category))?;
    let module = resolve_module_confident(db, env.resolver_environment(db), &module_name)?;
    let file = ProgramFile::new(db, module.file(db)?, env.program(db));
    blender_module_name(db, file)?;
    let operator = imported_symbol(db, env, Some(file), &idname.name, None)
        .place
        .ignore_possibly_undefined()?;
    let function = match operator {
        Type::FunctionLiteral(function) => function,
        Type::ClassLiteral(_) => match operator
            .member_lookup_with_policy(
                db,
                env,
                "__new__",
                MemberLookupPolicy::MRO_NO_OBJECT_FALLBACK,
            )
            .place
            .ignore_possibly_undefined()?
        {
            Type::FunctionLiteral(function) => function,
            _ => return None,
        },
        _ => return None,
    };
    let signature = function.signature(db).iter().next()?;
    let properties = signature
        .parameters()
        .iter()
        .filter(|parameter| parameter.is_keyword_only())
        .filter_map(|parameter| {
            let write = without_none(db, env, parameter.annotated_type());
            Some(FoundProperty {
                name: parameter.name()?.clone(),
                value: vector_value(db, env, write),
                write: Some(write),
                is_enum: builtin_is_enum(db, env, write),
            })
        })
        .collect();
    Some(FoundOperator {
        bl_form: idname.bl_form(),
        properties,
        origin: OperatorOrigin::Builtin(operator),
    })
}

/// `ty` without its `None` arm.
fn without_none<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> Type<'db> {
    match ty {
        Type::Union(union) => UnionType::from_elements(
            db,
            env,
            union
                .elements(db)
                .iter()
                .filter(|element| !element.is_none(db)),
        ),
        ty => ty,
    }
}

/// The `mathutils` arm of a vector parameter's annotation (`Vector | Sequence[float]`), or the
/// annotation itself.
fn vector_value<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> Type<'db> {
    let Type::Union(union) = ty else {
        return ty;
    };
    union
        .elements(db)
        .iter()
        .copied()
        .find(|element| {
            receiver_class(db, env, *element).is_some_and(|class| {
                matches!(*element, Type::NominalInstance(_))
                    && file_to_module(db, class.program_file(db).resolver_file(db)).is_some_and(
                        |module| {
                            module.name(db).as_str() == "mathutils"
                                || module.name(db).as_str().starts_with("mathutils.")
                        },
                    )
            })
        })
        .unwrap_or(ty)
}

/// Whether a built-in operator's property is an enum: a string literal or a set of them is; a
/// number or a boolean is not; a plain `str` may be a dynamic enum.
fn builtin_is_enum<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<bool> {
    if literal_strings_only(db, ty) {
        return Some(true);
    }
    if let Some((class, specialization)) = ty.class_specialization(db, env)
        && class.is_known(db, KnownClass::Set)
    {
        return Some(
            specialization
                .types(db)
                .iter()
                .all(|element| literal_strings_only(db, *element)),
        );
    }
    if ty.is_assignable_to(db, env, KnownClass::Float.to_instance(db, env)) {
        return Some(false);
    }
    None
}

fn literal_strings_only(db: &dyn Db, ty: Type<'_>) -> bool {
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

/// The properties type of an operator: `OperatorProperties & P`.
pub(super) fn properties_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    operator: &FoundOperator<'db>,
) -> Option<Type<'db>> {
    let nominal = module_class(db, env, "bpy.types", "OperatorProperties")?;
    let nominal = Type::instance(db, env, nominal.default_specialization(db));
    let identity = Type::string_literal(db, operator.bl_form.as_str());
    let members = operator
        .properties
        .iter()
        .map(|property| (property.name.as_str(), property.value))
        .chain([(IDENTITY, identity)]);
    let protocol = Type::protocol_with_readonly_members(db, env, members);
    Some(IntersectionType::from_elements(
        db,
        env,
        [nominal, protocol],
    ))
}

/// A key-map item or a macro step of an operator: `holder & K`, where `K` holds the operator's
/// properties.
fn holder_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    holder: Type<'db>,
    operator: &FoundOperator<'db>,
) -> Option<Type<'db>> {
    let properties = properties_type(db, env, operator)?;
    let identity = Type::string_literal(db, operator.bl_form.as_str());
    let protocol = Type::protocol_with_readonly_members(
        db,
        env,
        [("properties", properties), (IDENTITY, identity)],
    );
    Some(IntersectionType::from_elements(db, env, [holder, protocol]))
}

/// Refine the return type of a call to a sink, and check its idname and property arguments.
///
/// Only the arms of the callee that are sinks change: `self.layout.operator(...)` calls a
/// `UILayout | None` attribute.
pub(crate) fn on_call<'db>(
    context: &InferContext<'db, '_>,
    call: &ast::ExprCall,
    bindings: &mut Bindings<'db>,
    expression_type: &dyn Fn(&Expr) -> Option<Type<'db>>,
) {
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let mut checked = false;
    for callable in bindings.iter_flat_mut() {
        let Some(sink) = sink_of(db, env, callable.callable_type) else {
            continue;
        };
        for (_, binding) in callable.matching_overloads_mut() {
            let parameters = binding.signature.parameters();
            let argument_type = |name: &str| {
                let index = parameters
                    .iter()
                    .position(|parameter| parameter.name().is_some_and(|n| n == name))?;
                binding.parameter_types().get(index).copied().flatten()
            };
            let Some(idname_type) = argument_type(sink.parameter) else {
                continue;
            };
            let property_type = argument_type("property");
            let idname_argument = call.arguments.find_argument_value(sink.parameter, 0);
            let Some(idname) =
                argument_idname(db, env, idname_type, idname_argument, expression_type)
            else {
                continue;
            };
            let operator = find_operator(db, env, file, &idname);
            if !checked {
                checked = true;
                check_call(
                    context,
                    sink,
                    call,
                    idname_argument,
                    &idname,
                    operator.as_ref(),
                    property_type,
                );
            }
            let Some(operator) = operator else {
                continue;
            };
            let refined = match sink.result {
                SinkResult::Properties => properties_type(db, env, &operator),
                SinkResult::Holder => {
                    holder_type(db, env, without_none(db, env, binding.return_ty), &operator)
                }
                SinkResult::Unchanged => None,
            };
            if let Some(refined) = refined {
                binding.set_return_type(refined);
            }
        }
    }
}

/// Report an unknown idname, and a `property` argument that names no enum property.
fn check_call<'db>(
    context: &InferContext<'db, '_>,
    sink: Sink,
    call: &ast::ExprCall,
    idname_argument: Option<&Expr>,
    idname: &OperatorIdname,
    operator: Option<&FoundOperator<'db>>,
    property_type: Option<Type<'db>>,
) {
    let db = context.db();
    let Some(operator) = operator else {
        if let Some(argument) = idname_argument
            && reports_unknown_operators(db, context.program_file())
        {
            report(
                context,
                &BLENDER_UNKNOWN_OPERATOR,
                argument.range(),
                format!(
                    "No operator `{}` is registered by this add-on or built into Blender",
                    idname.py_form()
                ),
            );
        }
        return;
    };
    if !sink.enum_property {
        return;
    }
    let Some(property) = property_type.and_then(|ty| literal_string(db, ty)) else {
        return;
    };
    let range = call
        .arguments
        .find_argument_value("property", 1)
        .map_or_else(|| call.range(), Ranged::range);
    let message = match operator
        .properties
        .iter()
        .find(|candidate| candidate.name == property.as_str())
    {
        None => format!(
            "Operator `{}` has no property `{property}`",
            idname.py_form()
        ),
        Some(found) if found.is_enum == Some(false) => format!(
            "Property `{property}` of operator `{}` is not an enum property",
            idname.py_form()
        ),
        Some(_) => return,
    };
    report(context, &BLENDER_INVALID_OPERATOR_PROPERTY, range, message);
}

fn report(
    context: &InferContext<'_, '_>,
    lint: &'static LintMetadata,
    range: TextRange,
    message: String,
) {
    if let Some(builder) = context.report_lint(lint, range) {
        builder.into_diagnostic(message);
    }
}

/// Whether an unknown idname is reported in `file`: only in an add-on whose `register()` was
/// followed to its end, since the operator may be registered where the walk could not look, and a
/// file outside an add-on cannot know which add-on registers what.
fn reports_unknown_operators<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> bool {
    addon_in_program(db, file).is_some_and(|key| addon_operators(db, key).complete)
}

/// The protocol half of a refined receiver (`OperatorProperties & P`, `KeyMapItem & K`), with the
/// other elements.
fn refined_parts<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    receiver: Type<'db>,
) -> Option<(Type<'db>, String, Vec<Type<'db>>)> {
    let Type::Intersection(intersection) = receiver else {
        return None;
    };
    let mut protocol = None;
    let mut others = Vec::new();
    for element in intersection.positive(db) {
        if protocol.is_none()
            && let Some(idname) = identity(db, env, *element)
        {
            protocol = Some((*element, idname));
        } else {
            others.push(*element);
        }
    }
    let (protocol, idname) = protocol?;
    Some((protocol, idname, others))
}

/// The idname held by the identity member of a synthesised protocol.
fn identity<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> Option<String> {
    let Type::ProtocolInstance(_) = ty else {
        return None;
    };
    let member = ty
        .member(db, env, IDENTITY)
        .place
        .ignore_possibly_undefined()?;
    literal_string(db, member)
}

/// The attribute `name` of a refined receiver: the protocol's member, or a member the nominal
/// class defines itself (never through its `__getattr__`). The identity member is hidden. A union
/// with a refined arm reads every arm. `None` when the receiver is not refined.
pub(super) fn member<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Option<PlaceAndQualifiers<'db>> {
    if let Type::Union(union) = receiver {
        if !union
            .elements(db)
            .iter()
            .any(|element| refined_parts(db, env, *element).is_some())
        {
            return None;
        }
        let mut types = Vec::new();
        for element in union.elements(db) {
            let place =
                member(db, env, *element, name).unwrap_or_else(|| element.member(db, env, name));
            types.push(place.place.ignore_possibly_undefined()?);
        }
        return Some(Place::bound(UnionType::from_elements(db, env, types)).into());
    }
    let (protocol, _, others) = refined_parts(db, env, receiver)?;
    if name == IDENTITY {
        return Some(Place::Undefined.into());
    }
    let own = protocol.member(db, env, name);
    if own.place.is_definitely_bound() {
        return Some(own);
    }
    for other in others {
        let place =
            other.member_lookup_with_policy(db, env, name, MemberLookupPolicy::NO_GETATTR_LOOKUP);
        if !place.place.is_undefined() {
            return Some(place);
        }
    }
    Some(Place::Undefined.into())
}

/// `self.properties` of an add-on operator (or `step.properties` for an instance): the
/// properties of its class.
pub(super) fn own_properties<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
) -> Option<Type<'db>> {
    if !matches!(receiver, Type::NominalInstance(_) | Type::TypeVar(_)) {
        return None;
    }
    let class = receiver_class(db, env, receiver)?;
    if !is_operator_class(db, class) {
        return None;
    }
    let idname = operator_idname(db, class);
    let registered = idname.as_ref().and_then(|idname| {
        let key = addon_in_program(db, file)?;
        addon_operators(db, key)
            .get(&idname.category, &idname.name)
            .filter(|operator| operator.class == class)
            .map(|operator| addon_operator(db, env, file, operator, 0))
    });
    let operator = registered.unwrap_or_else(|| {
        let bl_form = idname.map_or_else(|| class.name(db).to_string(), |idname| idname.bl_form());
        class_operator(db, env, class, bl_form)
    });
    properties_type(db, env, &operator)
}

/// What a write to `name` of a refined receiver accepts: `Some(None)` for a read-only property,
/// `None` when the receiver is not refined or has no such property.
///
/// The operator is looked up again through the identity member in `file`. When that fails (the
/// properties came from an operator the file cannot see), the property's value type is used.
pub(super) fn write_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Option<Option<Type<'db>>> {
    if let Type::Union(union) = receiver {
        let mut write = None;
        for element in union.elements(db) {
            if element.is_dynamic() {
                continue;
            }
            let element_write = write_type(db, env, file, *element, name)?;
            if write.is_some_and(|write| write != element_write) {
                return None;
            }
            write = Some(element_write);
        }
        return write;
    }
    let (protocol, idname, _) = refined_parts(db, env, receiver)?;
    if name == IDENTITY {
        return None;
    }
    let value = protocol
        .member(db, env, name)
        .place
        .ignore_possibly_undefined()?;
    let found = OperatorIdname::from_lookup(&idname)
        .and_then(|idname| find_operator(db, env, file, &idname))
        .filter(|operator| operator.bl_form == idname)
        .and_then(|operator| {
            operator
                .properties
                .into_iter()
                .find(|property| property.name == name)
        });
    Some(match found {
        Some(property) => property.write,
        None => Some(value),
    })
}

/// The property names that complete `receiver.` for a refined receiver.
pub(super) fn completions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
) -> Vec<Member<'db>> {
    let Some((_, idname, _)) = refined_parts(db, env, receiver) else {
        return Vec::new();
    };
    let Some(operator) = OperatorIdname::from_lookup(&idname)
        .and_then(|idname| find_operator(db, env, file, &idname))
    else {
        return Vec::new();
    };
    operator
        .properties
        .into_iter()
        .map(|property| Member {
            name: property.name,
            ty: property.value,
            is_type_check_only: false,
        })
        .collect()
}

/// The sink call in `file` whose idname argument is the string literal that `matches` accepts,
/// with that string.
fn idname_string_at<'db, 'm>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    module: &'m ParsedModuleRef,
    matches: impl Fn(&ast::ExprStringLiteral) -> bool,
) -> Option<(&'m ast::ExprStringLiteral, Sink)> {
    struct Finder<'m, F> {
        matches: F,
        found: Option<(&'m ast::ExprCall, &'m ast::ExprStringLiteral)>,
    }
    impl<'m, F: Fn(&ast::ExprStringLiteral) -> bool> SourceOrderVisitor<'m> for Finder<'m, F> {
        fn visit_expr(&mut self, expr: &'m Expr) {
            if self.found.is_some() {
                return;
            }
            if let Expr::Call(call) = expr {
                let argument = call
                    .arguments
                    .find_argument_value("operator", 0)
                    .or_else(|| {
                        call.arguments
                            .find_keyword("idname")
                            .map(|keyword| &keyword.value)
                    });
                if let Some(Expr::StringLiteral(string)) = argument
                    && (self.matches)(string)
                {
                    self.found = Some((call, string));
                    return;
                }
            }
            walk_expr(self, expr);
        }
    }
    let mut finder = Finder {
        matches,
        found: None,
    };
    finder.visit_body(module.suite());
    let (call, string) = finder.found?;
    let env = ProgramEnvironment::from_file(file);
    let index = semantic_index(db, file);
    let scope = index
        .try_expression_scope_id(&*call.func)?
        .to_scope_id(db, file);
    let callee = infer_complete_scope_types(db, scope).try_expression_type(&*call.func)?;
    let sink = match callee {
        Type::Union(union) => union
            .elements(db)
            .iter()
            .find_map(|element| sink_of(db, &env, *element)),
        callee => sink_of(db, &env, callee),
    }?;
    Some((string, sink))
}

/// The operator idnames that complete the string `string` when it is the idname argument of a
/// sink: the add-on's operators, then Blender's in the categories the text so far can still name,
/// all in the `category.name` form.
pub(crate) fn idname_completions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    string: &ast::ExprStringLiteral,
) -> Vec<ExpectedStringLiteralCompletion<'db>> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    if idname_string_at(db, file, &module, |candidate| {
        candidate.range() == string.range()
    })
    .is_none()
    {
        return Vec::new();
    }
    let typed = string.value.to_str();
    let mut idnames: Vec<String> = addon_in_program(db, file)
        .map(|key| {
            addon_operators(db, key)
                .operators
                .iter()
                .map(|operator| operator.idname().py_form())
                .collect()
        })
        .unwrap_or_default();
    idnames.extend(builtin_idnames(db, env, typed));
    idnames
        .into_iter()
        .map(|value| ExpectedStringLiteralCompletion {
            ty: Type::string_literal(db, value.as_str()),
            value,
        })
        .collect()
}

/// Blender's operators in the `category.name` form, from the `bpy.ops` stubs, in the categories
/// that `typed` (the text of the string so far) can still name.
fn builtin_idnames<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    typed: &str,
) -> Vec<String> {
    let module_file = |name: &str| {
        let module =
            resolve_module_confident(db, env.resolver_environment(db), &ModuleName::new(name)?)?;
        let file = ProgramFile::new(db, module.file(db)?, env.program(db));
        blender_module_name(db, file).map(|_| file)
    };
    let public_names = |file: ProgramFile<'db>| {
        place_table(db, global_scope(db, file))
            .symbols()
            .filter(|symbol| symbol.is_bound() && !symbol.name().starts_with('_'))
            .map(|symbol| symbol.name().clone())
            .collect::<Vec<Name>>()
    };
    let Some(ops) = module_file("bpy.ops") else {
        return Vec::new();
    };
    let mut idnames = Vec::new();
    for category in public_names(ops) {
        let reachable = match typed.split_once('.') {
            Some((typed_category, _)) => typed_category == category.as_str(),
            None => category.starts_with(typed),
        };
        if !reachable {
            continue;
        }
        let Some(category_file) = module_file(&format!("bpy.ops.{category}")) else {
            continue;
        };
        for name in public_names(category_file) {
            let is_operator = matches!(
                imported_symbol(db, env, Some(category_file), &name, None)
                    .place
                    .ignore_possibly_undefined(),
                Some(Type::FunctionLiteral(_) | Type::ClassLiteral(_))
            );
            if is_operator {
                idnames.push(format!("{category}.{name}"));
            }
        }
    }
    idnames
}

/// What hovering an operator idname string shows: the operator's two idname forms, its label and
/// its description. `None` when the string at `offset` is not the idname argument of a sink, or
/// names no operator.
pub fn blender_idname_hover<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    offset: TextSize,
) -> Option<(TextRange, String)> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    let (string, _) = idname_string_at(db, file, &module, |candidate| {
        candidate.range().contains_inclusive(offset)
    })?;
    let env = ProgramEnvironment::from_file(file);
    let idname = OperatorIdname::from_lookup(string.value.to_str())?;
    let operator = find_operator(db, &env, file, &idname)?;
    let (label, description) = match operator.origin {
        OperatorOrigin::Addon(class) => (
            class_string(db, class, "bl_label"),
            class_string(db, class, "bl_description")
                .or_else(|| class.definition(db).docstring(db)),
        ),
        OperatorOrigin::Builtin(ty) => {
            let definition = match ty {
                Type::FunctionLiteral(function) => Some(function.definition(db)),
                Type::ClassLiteral(class) => class.as_static().map(|class| class.definition(db)),
                _ => None,
            };
            (
                None,
                definition.and_then(|definition| definition.docstring(db)),
            )
        }
    };
    let mut text = format!("Operator `{}` (`{}`)", idname.py_form(), operator.bl_form);
    if let Some(label) = label {
        text.push_str(&format!(": {label}"));
    }
    if let Some(description) = description {
        text.push_str("\n\n");
        text.push_str(description.trim());
    }
    Some((string.range(), text))
}

/// Check the `bl_keymap` tuples of the `WorkSpaceTool` classes in a file: each item's idname, and
/// the `(name, value)` pairs of its `"properties"`.
///
/// Blender turns each `(idname, event, data)` item into `keymap_items.new(idname, **event)` and
/// `setattr`s each pair on its properties (`bl_keymap_utils.io.keymap_init_from_data`); an unknown
/// name or a wrong value only prints a warning there.
pub(super) fn check_keymaps(context: &InferContext<'_, '_>, module: &ParsedModuleRef) {
    let db = context.db();
    let file = context.program_file();
    let index = semantic_index(db, file);
    for statement in module.suite() {
        let ast::Stmt::ClassDef(class_def) = statement else {
            continue;
        };
        let scope = index
            .node_scope(NodeWithScopeRef::Class(class_def))
            .to_scope_id(db, file);
        let Some(class) = nearest_enclosing_class(db, index, scope) else {
            continue;
        };
        if !bpy_types_bases(db, class).any(|base| base == "WorkSpaceTool") {
            continue;
        }
        let inference = infer_complete_scope_types(db, scope);
        let expression_type = |expression: &Expr| inference.try_expression_type(expression);
        for statement in &class_def.body {
            let ast::Stmt::Assign(assign) = statement else {
                continue;
            };
            if !assign.targets.iter().any(|target| {
                target
                    .as_name_expr()
                    .is_some_and(|name| name.id == "bl_keymap")
            }) {
                continue;
            }
            let Expr::Tuple(items) = &*assign.value else {
                continue;
            };
            for item in &items.elts {
                check_keymap_item(context, item, &expression_type);
            }
        }
    }
}

fn check_keymap_item<'db>(
    context: &InferContext<'db, '_>,
    item: &Expr,
    expression_type: &dyn Fn(&Expr) -> Option<Type<'db>>,
) {
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let Expr::Tuple(item) = item else {
        return;
    };
    let [idname_expression, _, data] = &*item.elts else {
        return;
    };
    let Some(idname) = expression_type(idname_expression)
        .and_then(|ty| argument_idname(db, env, ty, Some(idname_expression), expression_type))
    else {
        return;
    };
    let Some(operator) = find_operator(db, env, file, &idname) else {
        if reports_unknown_operators(db, file) {
            report(
                context,
                &BLENDER_UNKNOWN_OPERATOR,
                idname_expression.range(),
                format!(
                    "No operator `{}` is registered by this add-on or built into Blender",
                    idname.py_form()
                ),
            );
        }
        return;
    };
    let Expr::Dict(data) = data else {
        return;
    };
    let Some(properties) = data.items.iter().find_map(|item| match &item.key {
        Some(Expr::StringLiteral(key)) if key.value.to_str() == "properties" => Some(&item.value),
        _ => None,
    }) else {
        return;
    };
    let pairs = match properties {
        Expr::List(list) => &list.elts,
        Expr::Tuple(tuple) => &tuple.elts,
        _ => return,
    };
    for pair in pairs {
        let Expr::Tuple(pair_tuple) = pair else {
            continue;
        };
        let [Expr::StringLiteral(name), value] = &*pair_tuple.elts else {
            continue;
        };
        let name = name.value.to_str();
        let message = match operator
            .properties
            .iter()
            .find(|property| property.name == name)
        {
            None => format!("Operator `{}` has no property `{name}`", idname.py_form()),
            Some(property) => {
                // A list value sets the properties of a group pointer one by one.
                if matches!(value, Expr::List(_)) {
                    continue;
                }
                let (Some(write), Some(value_type)) = (property.write, expression_type(value))
                else {
                    continue;
                };
                if value_type.is_assignable_to(db, env, write) {
                    continue;
                }
                format!(
                    "Property `{name}` of operator `{}` takes `{}`, not `{}`",
                    idname.py_form(),
                    write.display(db, env),
                    value_type.display(db, env)
                )
            }
        };
        report(
            context,
            &BLENDER_INVALID_OPERATOR_PROPERTY,
            pair.range(),
            message,
        );
    }
}
