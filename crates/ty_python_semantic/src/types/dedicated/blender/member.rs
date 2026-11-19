//! Property members: the property behind an attribute of a Blender instance.
//!
//! An attribute such as `scene.my_value` or `group.count` can be a property of three origins:
//!
//! - an Annotated property, `count: IntProperty()` in the body of a Property host, which ty already
//!   sees as a class member (type-expression inference gives the annotation its value type);
//! - a Registered property, `bpy.types.Scene.my_value = IntProperty()` anywhere in the add-on, which
//!   no class body declares and which the [Add-on index](super::index) holds instead;
//! - a property that a class decorator adds: `@orientation_helper(...)` gives the class
//!   `axis_forward` and `axis_up`.
//!
//! All origins are resolved by [`instance_property`], so every rule about a property's value type,
//! writability and visibility is decided once, whatever the origin. The hooks into ty's member lookup,
//! attribute reads and writes, override checks and completions all go through it.

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::name::Name;
use ty_python_core::ProgramFile;
use ty_python_core::definition::{Definition, DefinitionState};
use ty_python_core::scope::ScopeId;
use ty_python_core::{place_table, semantic_index, use_def_map};

use crate::place::{Definedness, Place, PlaceAndQualifiers};
use crate::types::attribute_write::{
    AttributeWriteRequirement, ClassAttributeWriteMember, ExplicitAttributeWriteRequirement,
    FallbackAttributeWriteRequirement, InstanceAttributeWriteMember, attribute_write_requirement,
};
use crate::types::context::InferContext;
use crate::types::diagnostic::INVALID_ASSIGNMENT;
use crate::types::infer::nearest_enclosing_class;
use crate::types::list_members::Member;
use crate::types::{
    ClassBase, ClassType, IntersectionType, StaticClassLiteral, SubclassOfInner, Type,
    TypeQualifiers, TypeVarBoundOrConstraints, UnionType, definition_expression_type_in_scope,
};
use crate::{Db, ProgramEnvironment};

use super::evaluate::{PropertyValue, property_value};
use super::external::{ExternalAddon, ExternalSource, external_registration};
use super::follow::{Resolved, resolve_expression};
use super::index::{RegisteredProperty, registered_properties};
use super::mixin_self::mixin_users;
use super::properties::PropertyDeclaration;
use super::symbols::{blender_function_name, bpy_types_bases};
use super::{
    assumptions, context_assumptions, context_members, id_data, operator_properties, operators,
    pointer_aliases, poll_checks, preferences, subclass_members, temp_override,
};

/// The property that gives instances of `class` the attribute `name` in `file`.
///
/// The method resolution order is walked from the most derived class. The first class whose
/// decorator adds `name` decides, as does the first whose body declares or assigns `name`: its Annotated property, or nothing when it is a plain class
/// attribute, or when the class is an Ignored host (`value: IntProperty()` on a Panel creates no
/// property). A body that only reads the name (`alias = items`) does not decide. A class whose body
/// does not mention `name` gives way to the last registration of `name` on it in the add-on. A name
/// that no class decides comes from another add-on's registration ([`external_registration`]).
fn instance_property<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<&'db PropertyValue<'db>> {
    match own_instance_property(db, file, class, name) {
        Some(decided) => decided,
        None => external_registration(db, file, class, name).map(|(property, _)| &property.value),
    }
}

/// The add-on Blender enables by default whose registration gives instances of `class` the
/// attribute `name` in `file`, when [`instance_property`] takes it from one.
fn default_addon_property<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<&'db ExternalAddon<'db>> {
    if own_instance_property(db, file, class, name).is_some() {
        return None;
    }
    let (_, addon) = external_registration(db, file, class, name)?;
    (addon.source == ExternalSource::Default).then_some(addon)
}

/// The default add-on whose registration gives some arm of `receiver` the attribute `name` in
/// `file`, taken apart as [`instance_value_type`] does: an arm of a union, an element of an
/// intersection, a bound or constraint of a type variable.
fn default_addon_of<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Option<&'db ExternalAddon<'db>> {
    let first = |mut arms: &mut dyn Iterator<Item = &Type<'db>>| {
        Iterator::find_map(&mut arms, |arm| default_addon_of(db, env, file, *arm, name))
    };
    match receiver {
        Type::NominalInstance(instance) => {
            let class = instance.class(db, env).class_literal(db).as_static()?;
            default_addon_property(db, file, class, name)
        }
        Type::Union(union) => first(&mut union.elements(db).iter()),
        Type::Intersection(intersection) => first(&mut intersection.positive(db).iter()),
        Type::TypeVar(typevar) => match typevar.typevar(db).bound_or_constraints(db, env)? {
            TypeVarBoundOrConstraints::UpperBound(bound) => {
                default_addon_of(db, env, file, bound, name)
            }
            TypeVarBoundOrConstraints::Constraints(constraints) => {
                first(&mut constraints.elements(db).iter())
            }
        },
        Type::TypeAlias(alias) => default_addon_of(db, env, file, alias.value_type(db), name),
        _ => None,
    }
}

/// The property that the add-on of `file` gives instances of `class` for `name` (see
/// [`instance_property`]): `Some(None)` when a class decides there is none, `None` when no class
/// decides.
fn own_instance_property<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<Option<&'db PropertyValue<'db>>> {
    let registered = registered_properties(db, file);
    let registered = registered.iter().any(|property| property.name == name);
    class.iter_mro(db, None).find_map(|base| {
        let ClassBase::Class(base) = base else {
            return None;
        };
        let base = base.class_literal(db).as_static()?;
        if let Some(property) = decorator_property(db, base, name) {
            return Some(Some(property).filter(|_| !is_ignored_host(db, base)));
        }
        if body_binds(db, base, name) {
            return Some(annotated_property(db, base, name).filter(|_| !is_ignored_host(db, base)));
        }
        // A registration on a class that doesn't accept one (`bpy.types.Area.x = IntProperty()`)
        // creates no property.
        registered
            .then(|| registered_on(db, file, base, name))
            .flatten()
            .filter(|property| property.accepted)
            .map(|property| Some(&property.value))
    })
}

/// The two properties that `@bpy_extras.io_utils.orientation_helper(...)` adds to the class it
/// decorates.
const ORIENTATION_AXES: [&str; 2] = ["axis_forward", "axis_up"];

/// The property that `class` gets for `name` from its decorators. `@orientation_helper(...)` puts
/// `axis_forward` and `axis_up` into the class's own annotations, over any the body declares, as
/// enums of the six axes. No stub can express a decorator that adds attributes to a class.
fn decorator_property<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<&'db PropertyValue<'db>> {
    if !ORIENTATION_AXES.contains(&name) {
        return None;
    }
    orientation_axis_property(db, class).as_ref()
}

/// The properties that the decorators of `class` add to its annotations, in their order.
pub(super) fn decorator_properties<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Vec<(Name, Option<PropertyValue<'db>>)> {
    orientation_axis_property(db, class)
        .as_ref()
        .map(|property| {
            ORIENTATION_AXES
                .iter()
                .map(|axis| (Name::new_static(axis), Some(property.clone())))
                .collect()
        })
        .unwrap_or_default()
}

/// The six-axis enum property of `axis_forward` and `axis_up`, when `class` is decorated with
/// `@orientation_helper(...)`. The decorator is read from the class statement, and its callee's type
/// from the class definition's inference, so that any import of the function is recognised.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn orientation_axis_property<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<PropertyValue<'db>> {
    let module = parsed_module(db, class.program_file(db).python_file(db)).load(db);
    let body_scope = class.body_scope(db);
    let statement = body_scope.node(db).expect_class().node(&module);
    let definition = class.definition(db);
    let scope = definition.scope(db);
    statement
        .decorator_list
        .iter()
        .any(|decorator| {
            let ast::Expr::Call(call) = &decorator.expression else {
                return false;
            };
            let callee = definition_expression_type_in_scope(db, definition, &call.func, scope);
            blender_function_name(db, callee, "bpy_extras.io_utils") == Some("orientation_helper")
        })
        .then(|| {
            PropertyValue::Declarations(Box::new([PropertyDeclaration::static_enum(&[
                "X", "Y", "Z", "-X", "-Y", "-Z",
            ])]))
        })
}

/// The last registration of `name` on `owner` in `file`'s add-on.
fn registered_on<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    owner: StaticClassLiteral<'db>,
    name: &str,
) -> Option<&'db RegisteredProperty<'db>> {
    registered_properties(db, file)
        .iter()
        .rev()
        .find(|property| property.owner == owner && property.name == name)
}

/// The registration of `name` on the first class in the method resolution order of `class` that
/// has one in `file`'s add-on.
fn registration_on_class<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<&'db RegisteredProperty<'db>> {
    class.iter_mro(db, None).find_map(|base| match base {
        ClassBase::Class(base) => {
            registered_on(db, file, base.class_literal(db).as_static()?, name)
        }
        _ => None,
    })
}

/// Whether some class in the method resolution order of `class` has a registration of `name` in
/// `file`'s add-on. This is what makes `bpy.types.Scene.my_value` a known name of the class object,
/// for the registration itself and for `del` in `unregister()`.
fn is_registered_on_class<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> bool {
    if !registered_properties(db, file)
        .iter()
        .any(|property| property.name == name)
    {
        return false;
    }
    class.iter_mro(db, None).any(|base| match base {
        ClassBase::Class(base) => base
            .class_literal(db)
            .as_static()
            .is_some_and(|base| registered_on(db, file, base, name).is_some()),
        _ => false,
    })
}

/// Whether the body of `class` declares or assigns `name`.
fn body_binds<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>, name: &str) -> bool {
    let table = place_table(db, class.body_scope(db));
    table.symbol_id(name).is_some_and(|symbol| {
        let symbol = table.symbol(symbol);
        symbol.is_declared() || symbol.is_bound()
    })
}

/// The property that the class body of `class` declares as `name: SomeProperty(...)`.
pub(super) fn annotated_property<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<&'db PropertyValue<'db>> {
    let scope = class.body_scope(db);
    let symbol = place_table(db, scope).symbol_id(name)?;
    use_def_map(db, scope)
        .end_of_scope_symbol_declarations(symbol)
        .find_map(|declaration| match declaration.declaration {
            DefinitionState::Defined(definition) => property_value(db, definition).as_ref(),
            DefinitionState::Undefined | DefinitionState::Deleted => None,
        })
}

/// Whether Blender ignores the annotations of `class`: it derives from a `bpy.types` class, but
/// from none of the Property hosts (a Panel or a Menu, not a `PropertyGroup` or an Operator). A
/// plain class keeps its annotations, as it may be a mixin of a Property host.
pub(super) fn is_ignored_host<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    const PROPERTY_HOSTS: [&str; 11] = [
        "PropertyGroup",
        "Operator",
        "Macro",
        "AddonPreferences",
        "UIList",
        "Node",
        "NodeSocket",
        "NodeTree",
        "NodeTreeInterfaceSocket",
        "Gizmo",
        "KeyConfigPreferences",
    ];
    let mut bases = bpy_types_bases(db, class)
        .filter(|base| *base != "bpy_struct")
        .peekable();
    bases.peek().is_some() && !bases.any(|base| PROPERTY_HOSTS.contains(&base))
}

/// The value type of the attribute `name` read from `receiver` in `file`: ty's own where it finds
/// the attribute, otherwise the one a property ty does not see gives it ([`instance_value_type`]).
pub(super) fn attribute_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Option<Type<'db>> {
    match receiver.member(db, env, name).place {
        Place::Defined(defined) if defined.definedness == Definedness::AlwaysDefined => {
            Some(defined.ty)
        }
        _ => instance_value_type(db, env, file, receiver, name),
    }
}

/// The value type of the attribute `name` read from `receiver` in `file`, when a property that ty
/// does not see gives it one.
///
/// The receiver is taken apart the way ty's member lookup does: every arm of a union and every
/// constraint of a type variable must have the attribute, from its class or from a property; a
/// positive element of an intersection is enough; a bounded type variable reads its bound.
fn instance_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Option<Type<'db>> {
    // An arm that ty resolves itself keeps ty's type; the others need a property.
    let arm_type = |arm: Type<'db>| attribute_value_type(db, env, file, arm, name);
    let all_arms = |arms: &[Type<'db>]| {
        let types = arms
            .iter()
            .map(|arm| arm_type(*arm))
            .collect::<Option<Vec<_>>>()?;
        Some(UnionType::from_elements(db, env, types))
    };
    match receiver {
        Type::NominalInstance(instance) => {
            let class = instance.class(db, env).class_literal(db).as_static()?;
            Some(instance_property(db, file, class, name)?.value_type(db, env))
        }
        Type::Union(union) => all_arms(union.elements(db)),
        Type::Intersection(intersection) => {
            let types = intersection
                .positive(db)
                .iter()
                .filter_map(|element| arm_type(*element))
                .collect::<Vec<_>>();
            (!types.is_empty()).then(|| IntersectionType::from_elements(db, env, types))
        }
        Type::TypeVar(typevar) => match typevar.typevar(db).bound_or_constraints(db, env)? {
            TypeVarBoundOrConstraints::UpperBound(bound) => arm_type(bound),
            TypeVarBoundOrConstraints::Constraints(constraints) => {
                all_arms(constraints.elements(db))
            }
        },
        Type::TypeAlias(alias) => instance_value_type(db, env, file, alias.value_type(db), name),
        _ => None,
    }
}

/// What a write to the attribute `name` of the instance `receiver` in `file` accepts, when a
/// property of either origin gives it the attribute. A vector property accepts more than its value
/// type: `s.location = (0.0, 0.0, 1.0)` where `s.location` is a `Vector`.
fn instance_write_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Option<Type<'db>> {
    let Type::NominalInstance(instance) = receiver else {
        return None;
    };
    let class = instance.class(db, env).class_literal(db).as_static()?;
    Some(instance_property(db, file, class, name)?.write_type(db, env))
}

/// Whether writing the attribute `name` of `receiver` in `file` hits a read-only property on any
/// arm of the receiver: every arm of a union, element of an intersection and bound or constraint
/// of a type variable is written to.
fn is_read_only<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
    name: &str,
) -> bool {
    let any_arm = |mut arms: &mut dyn Iterator<Item = &Type<'db>>| {
        Iterator::any(&mut arms, |arm| is_read_only(db, env, file, *arm, name))
    };
    match receiver {
        Type::NominalInstance(instance) => instance
            .class(db, env)
            .class_literal(db)
            .as_static()
            .and_then(|class| instance_property(db, file, class, name))
            .is_some_and(|declaration| declaration.is_read_only(db)),
        Type::Union(union) => any_arm(&mut union.elements(db).iter()),
        Type::Intersection(intersection) => any_arm(&mut intersection.positive(db).iter()),
        Type::TypeVar(typevar) => match typevar.typevar(db).bound_or_constraints(db, env) {
            Some(TypeVarBoundOrConstraints::UpperBound(bound)) => {
                is_read_only(db, env, file, bound, name)
            }
            Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                any_arm(&mut constraints.elements(db).iter())
            }
            None => false,
        },
        Type::TypeAlias(alias) => is_read_only(db, env, file, alias.value_type(db), name),
        _ => false,
    }
}

/// The class behind a class-object receiver (`bpy.types.Scene`, `type[Scene]`).
fn class_receiver<'db>(db: &'db dyn Db, receiver: Type<'db>) -> Option<StaticClassLiteral<'db>> {
    match receiver {
        Type::ClassLiteral(class) => class.as_static(),
        Type::SubclassOf(subclass_of) => match subclass_of.subclass_of() {
            SubclassOfInner::Class(class) => class.class_literal(db).as_static(),
            _ => None,
        },
        _ => None,
    }
}

/// Whether the class attribute `name` of `class` is hidden because it is an Annotated property.
///
/// Blender replaces the class attribute with an RNA property when it registers the class, so
/// `MyGroup.count` is an `AttributeError` at runtime, while `my_group.count` is the value.
pub(crate) fn hides_class_member<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> bool {
    annotated_property(db, class, name).is_some()
}

/// Whether the instance attribute `name` of `class` is hidden because Blender ignores the
/// Annotated property that declares it: `value: IntProperty()` on a Panel creates nothing.
pub(crate) fn hides_instance_member<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> bool {
    annotated_property(db, class, name).is_some() && is_ignored_host(db, class)
}

/// Whether overriding `name` of `superclass` with `definition` is never an incompatible override,
/// because one of the two is an Annotated property.
///
/// A subclass may give an inherited property another type (`base_value: StringProperty()` over
/// `base_value: IntProperty()`), or replace it with a plain class attribute (`base_value = 5`),
/// and its instances then have the subclass's attribute.
pub(crate) fn exempts_attribute_override<'db>(
    db: &'db dyn Db,
    superclass: ClassType<'db>,
    name: &str,
    definition: Definition<'db>,
) -> bool {
    property_value(db, definition).is_some()
        || superclass
            .class_literal(db)
            .as_static()
            .is_some_and(|superclass| annotated_property(db, superclass, name).is_some())
}

/// The attribute `attribute` read from `receiver` in the file of `context`, where ty found `place`.
///
/// `context` is where a Blender rule that accepts a narrowed read reports it, at the read.
///
/// When ty finds nothing, or finds it on only some arms of the receiver, a Registered property in
/// the file's add-on gives the attribute its value type. On a class object only `del` is accepted
/// (`del bpy.types.Scene.my_value` in `unregister()`): a plain read gives the property declaration
/// at runtime, not the value, and stays unresolved.
///
/// It also replaces names ty does find: `preferences.addons[__package__].preferences` is the
/// add-on's own preferences class ([`preferences`]), and `bpy.ops` and operator idnames give the
/// add-on's operators.
pub(crate) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
    place: PlaceAndQualifiers<'db>,
) -> PlaceAndQualifiers<'db> {
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let name = attribute.attr.id.as_str();
    if name == "preferences"
        && let Some(ty) = preferences::preferences_load(db, env, file, attribute, receiver)
    {
        assumptions::report_preferences_read(context, attribute);
        return Place::bound(ty).into();
    }
    if attribute.ctx.is_load()
        && let Some(class) = operators::conditional_operator(db, file, attribute)
    {
        assumptions::report_conditional_class(
            context,
            attribute,
            &format!("`{}`", display_dotted(attribute)),
            class,
        );
    }
    if let Type::ModuleLiteral(module) = receiver
        && let Some(ty) = operators::module_attribute(
            db,
            env,
            file,
            module.module(db),
            name,
            place.place.is_definitely_bound(),
        )
    {
        return Place::bound(ty).into();
    }
    // `layout.operator("my_addon.run").count`: the operator's properties, also where the stub's
    // `__getattr__` accepts any name.
    if let Some(member) = operator_properties::load(context, attribute, receiver) {
        return member;
    }
    if name == "properties"
        && let Some(ty) = operator_properties::own_properties(db, env, file, receiver)
    {
        return Place::bound(ty).into();
    }
    if name == "layout" && is_drawn_layout(db, file, attribute) {
        return place.map_type(|ty| without_none(db, ty));
    }
    if name == "id_data"
        && let Some(ty) = id_data::id_data_type(db, env, receiver)
    {
        return Place::bound(ty).into();
    }
    // A Grease Pencil brush's settings, a member passed to `temp_override`.
    if let Some(member) = pointer_aliases::on_load(context, attribute, place) {
        return member;
    }
    if let Some(member) = temp_override::on_load(context, attribute, receiver, place) {
        return member;
    }
    // `context.scene` inside a Blender callback, and a pointer its class's `poll()` checked
    // (`context.object` in `draw`), narrowed further.
    let guaranteed = context_members::on_load(context, attribute, receiver, place);
    if let Some(member) = poll_checks::on_load(context, attribute, guaranteed.unwrap_or(place)) {
        return member;
    }
    if let Some(member) = guaranteed {
        return member;
    }
    if let Some(member) = context_assumptions::on_load(context, attribute, receiver, place) {
        return member;
    }
    if place.place.is_definitely_bound() {
        return place;
    }
    if let Some(class) = class_receiver(db, receiver) {
        return if attribute.ctx.is_del()
            && place.is_undefined()
            && is_registered_on_class(db, file, class, name)
        {
            Place::bound(Type::unknown()).into()
        } else {
            place
        };
    }
    match instance_value_type(db, env, file, receiver, name) {
        Some(ty) => {
            if attribute.ctx.is_load()
                && let Type::NominalInstance(instance) = receiver
                && let Some(class) = instance.class(db, env).class_literal(db).as_static()
                && let Some(property) = registration_on_class(db, file, class, name)
            {
                assumptions::report_conditional_property(
                    context,
                    attribute,
                    property.owner,
                    &property.name,
                );
            }
            if attribute.ctx.is_load()
                && let Some(addon) = default_addon_of(db, env, file, receiver, name)
            {
                assumptions::report_default_addon(context, attribute, &addon.name);
            }
            Place::bound(ty).into()
        }
        // `ob.constraints[0].target`: a member only some refined subclasses declare.
        None => subclass_members::on_load(context, attribute, receiver, place).unwrap_or(place),
    }
}

/// The dotted source of `expression`, as far as it is names and attributes (`bpy.ops.my.run`).
fn display_dotted(expression: &ast::ExprAttribute) -> String {
    match &*expression.value {
        ast::Expr::Name(name) => format!("{}.{}", name.id, expression.attr.id),
        ast::Expr::Attribute(value) => format!("{}.{}", display_dotted(value), expression.attr.id),
        _ => expression.attr.id.to_string(),
    }
}

/// The callbacks in which Blender gives `self.layout` a layout, by the `bpy.types` class that
/// declares them. Elsewhere (`poll`, a timer, `AddonPreferences.draw`, an Operator's `execute`) it
/// may be `None`.
const DRAW_CALLBACKS: [(&str, &[&str]); 1] = [("Operator", &["draw"])];

/// The `bpy.types` classes whose instances Python only ever sees while Blender draws them: a Panel,
/// Menu or Header instance exists only in its `draw` callbacks (`poll` is a classmethod), so every
/// instance method of such a class, `draw` or a helper it calls on `self`, runs while the layout is
/// set.
const DRAWN_CLASSES: [&str; 3] = ["Panel", "Menu", "Header"];

/// Whether `attribute` reads the layout of the instance being drawn: its receiver is the first
/// parameter (`self`) of a draw callback ([`is_draw_callback`]), in its body or in a comprehension
/// or lambda inside it. Another instance's `layout` (`other.layout`) may still be `None`.
fn is_drawn_layout<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
) -> bool {
    let ast::Expr::Name(receiver) = &*attribute.value else {
        return false;
    };
    let index = semantic_index(db, file);
    let Some(scope) = index.try_expression_scope_id(&*attribute.value) else {
        return false;
    };
    let Some((function, node)) = index
        .ancestor_scopes(scope)
        .find_map(|(id, ancestor)| Some((id, ancestor.node().as_function()?)))
    else {
        return false;
    };
    let module = parsed_module(db, file.python_file(db)).load(db);
    let parameters = &node.node(&module).parameters;
    let first = parameters.posonlyargs.iter().chain(&parameters.args).next();
    first.is_some_and(|first| first.parameter.name.id == receiver.id)
        && *is_draw_callback(db, function.to_scope_id(db, file))
}

/// Whether the function scope `function` is a draw callback: a method, in the body of a class
/// deriving from the `bpy.types` class that declares the callback ([`DRAW_CALLBACKS`]), or any
/// instance method of a class deriving from a Panel, Menu or Header ([`DRAWN_CLASSES`]). A
/// module-level function whose first parameter is annotated with such a class
/// (`def menu_func(self: Menu, context)`, a function given to `Menu.append`) is one too: its first
/// argument is an instance, which exists only while Blender draws it.
///
/// The callback is found by its name, not by registration: an unregistered base's `draw` is
/// inherited by the registered subclass that Blender calls it on. Since the hook changes the type
/// of the expression, `layout = self.layout` is a `UILayout` too.
#[salsa::tracked(cycle_initial=|_, _, _| false, heap_size=ruff_memory_usage::heap_size)]
fn is_draw_callback<'db>(db: &'db dyn Db, function: ScopeId<'db>) -> bool {
    let file = function.program_file(db);
    let index = semantic_index(db, file);
    let Some(parent) = index.parent_scope_id(function.file_scope_id(db)) else {
        return false;
    };
    let in_class_body = index.scope(parent).node().as_class().is_some();
    let Some(node) = function.node(db).as_function() else {
        return false;
    };
    let module = parsed_module(db, file.python_file(db)).load(db);
    let node = node.node(&module);
    if !in_class_body {
        return parent.is_global()
            && has_drawn_first_parameter(db, parent.to_scope_id(db, file), node);
    }
    let name = node.name.as_str();
    let Some(class) = nearest_enclosing_class(db, index, function) else {
        return false;
    };
    let is_instance_method = !node.decorator_list.iter().any(|decorator| {
        decorator
            .expression
            .as_name_expr()
            .is_some_and(|decorator| {
                matches!(decorator.id.as_str(), "classmethod" | "staticmethod")
            })
    });
    let is_drawn = |class| {
        bpy_types_bases(db, class).any(|base| {
            (is_instance_method && DRAWN_CLASSES.contains(&base))
                || DRAW_CALLBACKS
                    .iter()
                    .any(|(root, callbacks)| *root == base && callbacks.contains(&name))
        })
    };
    is_drawn(class) || {
        // A mixin's method is drawn when it is for every user of the mixin.
        let users = mixin_users(db, class);
        !users.is_empty() && users.iter().all(|user| is_drawn(*user))
    }
}

/// Whether the first parameter of the module-level function `node`, defined in `scope`, is
/// annotated with a class deriving from a Panel, Menu or Header ([`DRAWN_CLASSES`]).
fn has_drawn_first_parameter<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    node: &ast::StmtFunctionDef,
) -> bool {
    let parameters = &node.parameters;
    let Some(annotation) = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .next()
        .and_then(|first| first.parameter.annotation.as_deref())
    else {
        return false;
    };
    let Some(Resolved::Value(Type::ClassLiteral(class))) =
        resolve_expression(db, scope, annotation)
    else {
        return false;
    };
    class
        .as_static()
        .is_some_and(|class| bpy_types_bases(db, class).any(|base| DRAWN_CLASSES.contains(&base)))
}

/// `ty` without `None`.
pub(super) fn without_none<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
    match ty {
        Type::Union(union) => union.filter(db, |element| !element.is_none(db)),
        _ => ty,
    }
}

/// The write requirement of `object_ty.attribute = ...` in `file`, given the one ty computed.
///
/// Where ty finds no declared member, a Registered property in the file's add-on decides: an
/// instance write must be assignable to the property's write type, and a write to the class object
/// (the registration itself) is unconstrained. Where ty finds the instance attribute that an
/// Annotated property declares, which has the value type, the write type replaces it. The two
/// differ only for a vector property. Read-only properties are rejected afterwards by [`check_property_write`].
pub(in crate::types) fn on_store<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &str,
    requirement: AttributeWriteRequirement<'db>,
) -> AttributeWriteRequirement<'db> {
    // `layout.operator("my_addon.run").count = 1` writes an operator property.
    if let AttributeWriteRequirement::Any { object_ty, .. }
    | AttributeWriteRequirement::All { object_ty, .. } = requirement
        && let Some(write) = operator_properties::write_type(db, env, file, object_ty, attribute)
    {
        return match write {
            Some(ty) => AttributeWriteRequirement::Instance {
                object_ty,
                receiver: None,
                member: InstanceAttributeWriteMember::Explicit {
                    member: ExplicitAttributeWriteRequirement::AssignableTo {
                        ty,
                        qualifiers: TypeQualifiers::empty(),
                    },
                    fallback: None,
                },
            },
            None => AttributeWriteRequirement::CannotAssign,
        };
    }
    // `coll.my_prop = 1` after `if coll:` (`BoneCollection & ~AlwaysFalsy`) or `hasattr`: ty
    // resolves the write on the positive elements of the intersection later, without this hook, so
    // the element that has the property decides here.
    if let AttributeWriteRequirement::Any {
        intersection,
        receiver,
        ..
    } = requirement
        && let Some(element) = intersection
            .positive(db)
            .iter()
            .find(|element| instance_write_type(db, env, file, **element, attribute).is_some())
    {
        return on_store(
            db,
            env,
            file,
            attribute,
            attribute_write_requirement(db, env, *element, attribute, receiver),
        );
    }
    match requirement {
        AttributeWriteRequirement::Instance {
            object_ty,
            receiver,
            member: InstanceAttributeWriteMember::SetAttr,
        } if let Some(ty) = instance_write_type(db, env, file, object_ty, attribute)
            .or_else(|| instance_value_type(db, env, file, object_ty, attribute)) =>
        {
            AttributeWriteRequirement::Instance {
                object_ty,
                receiver,
                member: InstanceAttributeWriteMember::Explicit {
                    member: ExplicitAttributeWriteRequirement::AssignableTo {
                        ty,
                        qualifiers: TypeQualifiers::empty(),
                    },
                    fallback: None,
                },
            }
        }
        // `count: IntProperty()` without a value declares an instance attribute, not a class one.
        AttributeWriteRequirement::Instance {
            object_ty,
            receiver,
            member:
                InstanceAttributeWriteMember::Instance(
                    FallbackAttributeWriteRequirement::AssignableTo {
                        qualifiers,
                        possibly_missing,
                        ..
                    },
                ),
        } if let Some(ty) = instance_write_type(db, env, file, object_ty, attribute) => {
            AttributeWriteRequirement::Instance {
                object_ty,
                receiver,
                member: InstanceAttributeWriteMember::Instance(
                    FallbackAttributeWriteRequirement::AssignableTo {
                        ty,
                        qualifiers,
                        possibly_missing,
                    },
                ),
            }
        }
        AttributeWriteRequirement::Class {
            object_ty,
            member: ClassAttributeWriteMember::Unresolved { .. },
        } if class_receiver(db, object_ty)
            .is_some_and(|class| is_registered_on_class(db, file, class, attribute)) =>
        {
            AttributeWriteRequirement::Unconstrained
        }
        // The stubs, generated from a running Blender, declare the properties its startup scripts
        // register (`WindowManager.preset_name: str`): the registration itself is the write.
        AttributeWriteRequirement::Class {
            object_ty,
            member:
                ClassAttributeWriteMember::ClassAttribute(
                    FallbackAttributeWriteRequirement::AssignableTo { ty, .. },
                ),
        } if class_receiver(db, object_ty)
            .and_then(|class| registration_on_class(db, file, class, attribute))
            .is_some_and(|property| {
                property
                    .value
                    .value_type(db, env)
                    .is_assignable_to(db, env, ty)
            }) =>
        {
            AttributeWriteRequirement::Unconstrained
        }
        requirement => requirement,
    }
}

/// Check a write `target = ...` to the attribute `attribute` of `object_ty`, which ty's own checks
/// have accepted, against a read-only property of either origin. Reports an `invalid-assignment`
/// and returns `false` when the property is read-only.
///
/// Blender's read-only properties behave like a property with a getter and no setter: writing
/// `group.items = []` or `scene.my_group = other` raises `AttributeError`.
pub(crate) fn check_property_write<'db>(
    context: &InferContext<'db, '_>,
    env: &ProgramEnvironment<'db>,
    target: &ast::ExprAttribute,
    object_ty: Type<'db>,
    attribute: &str,
    emit_diagnostics: bool,
) -> bool {
    let db = context.db();
    if !is_read_only(db, env, context.program_file(), object_ty, attribute) {
        return true;
    }
    if emit_diagnostics && let Some(builder) = context.report_lint(&INVALID_ASSIGNMENT, target) {
        builder.into_diagnostic(format_args!(
            "Property `{attribute}` defined in `{}` is read-only",
            object_ty.display(db, env)
        ));
    }
    false
}

/// The Registered properties and the properties added by a decorator that complete `receiver.` in
/// `file`, with their value types.
pub(crate) fn completions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
) -> Vec<Member<'db>> {
    let mut members: Vec<Member<'db>> = operators::completions(db, env, file, receiver);
    members.extend(operator_properties::completions(db, env, file, receiver));
    let axes = ORIENTATION_AXES.map(Name::new_static);
    let names = registered_properties(db, file)
        .iter()
        .map(|property| &property.name)
        .chain(&axes);
    for name in names {
        if members.iter().any(|member| &member.name == name)
            || receiver.member(db, env, name).place.is_definitely_bound()
        {
            continue;
        }
        if let Some(ty) = instance_value_type(db, env, file, receiver, name) {
            members.push(Member {
                name: name.clone(),
                ty,
                is_type_check_only: false,
            });
        }
    }
    members
}
