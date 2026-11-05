//! Property members: the property behind an attribute of a Blender instance.
//!
//! An attribute such as `scene.my_value` or `group.count` can be a property of two origins:
//!
//! - an Annotated property, `count: IntProperty()` in the body of a Property host, which ty already
//!   sees as a class member (type-expression inference gives the annotation its value type);
//! - a Registered property, `bpy.types.Scene.my_value = IntProperty()` anywhere in the add-on, which
//!   no class body declares and which the [Add-on index](super::index) holds instead.
//!
//! Both origins are resolved by [`instance_property`], so every rule about a property's value type,
//! writability and visibility is decided once, whatever the origin. The hooks into ty's member lookup,
//! attribute reads and writes, override checks and completions all go through it.

use ruff_python_ast as ast;
use ty_python_core::ProgramFile;
use ty_python_core::definition::{Definition, DefinitionState};
use ty_python_core::{place_table, use_def_map};

use crate::place::{Definedness, Place, PlaceAndQualifiers};
use crate::types::attribute_write::{
    AttributeWriteRequirement, ClassAttributeWriteMember, ExplicitAttributeWriteRequirement,
    InstanceAttributeWriteMember,
};
use crate::types::context::InferContext;
use crate::types::diagnostic::INVALID_ASSIGNMENT;
use crate::types::list_members::Member;
use crate::types::{
    ClassBase, ClassType, IntersectionType, StaticClassLiteral, SubclassOfInner, Type,
    TypeQualifiers, TypeVarBoundOrConstraints, UnionType,
};
use crate::{Db, ProgramEnvironment};

use super::index::{RegisteredProperty, registered_properties};
use super::properties::{PropertyDeclaration, property_declaration};
use super::symbols::bpy_types_bases;

/// The property that gives instances of `class` the attribute `name` in `file`.
///
/// The method resolution order is walked from the most derived class. The first class whose body
/// declares or assigns `name` decides: its Annotated property, or nothing when it is a plain class
/// attribute, or when the class is an Ignored host (`value: IntProperty()` on a Panel creates no
/// property). A body that only reads the name (`alias = items`) does not decide. A class whose body
/// does not mention `name` gives way to the last registration of `name` on it in the add-on.
fn instance_property<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<&'db PropertyDeclaration<'db>> {
    let registered = registered_properties(db, file);
    let registered = registered.iter().any(|property| property.name == name);
    class.iter_mro(db, None).find_map(|base| {
        let ClassBase::Class(base) = base else {
            return None;
        };
        let base = base.class_literal(db).as_static()?;
        if body_binds(db, base, name) {
            return Some(annotated_property(db, base, name).filter(|_| !is_ignored_host(db, base)));
        }
        registered
            .then(|| registered_on(db, file, base, name))
            .flatten()
            .map(|property| Some(&property.declaration))
    })?
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
fn annotated_property<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<&'db PropertyDeclaration<'db>> {
    let scope = class.body_scope(db);
    let symbol = place_table(db, scope).symbol_id(name)?;
    use_def_map(db, scope)
        .end_of_scope_symbol_declarations(symbol)
        .find_map(|declaration| match declaration.declaration {
            DefinitionState::Defined(definition) => property_declaration(db, definition).as_ref(),
            DefinitionState::Undefined | DefinitionState::Deleted => None,
        })
}

/// Whether Blender ignores the annotations of `class`: it derives from a `bpy.types` class, but
/// from none of the Property hosts (a Panel or a Menu, not a `PropertyGroup` or an Operator). A
/// plain class keeps its annotations, as it may be a mixin of a Property host.
fn is_ignored_host<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    const PROPERTY_HOSTS: [&str; 9] = [
        "PropertyGroup",
        "Operator",
        "Macro",
        "AddonPreferences",
        "UIList",
        "Node",
        "NodeSocket",
        "NodeTree",
        "Gizmo",
    ];
    let mut bases = bpy_types_bases(db, class)
        .filter(|base| *base != "bpy_struct")
        .peekable();
    bases.peek().is_some() && !bases.any(|base| PROPERTY_HOSTS.contains(&base))
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
    let arm_type = |arm: Type<'db>| match arm.member(db, env, name).place {
        Place::Defined(defined) if defined.definedness == Definedness::AlwaysDefined => {
            Some(defined.ty)
        }
        _ => instance_value_type(db, env, file, arm, name),
    };
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
    property_declaration(db, definition).is_some()
        || superclass
            .class_literal(db)
            .as_static()
            .is_some_and(|superclass| annotated_property(db, superclass, name).is_some())
}

/// The attribute `attribute` read from `receiver` in `file`, where ty found `place`.
///
/// When ty finds nothing, or finds it on only some arms of the receiver, a Registered property in
/// the file's add-on gives the attribute its value type. On a class object only `del` is accepted
/// (`del bpy.types.Scene.my_value` in `unregister()`): a plain read gives the property declaration
/// at runtime, not the value, and stays unresolved.
pub(crate) fn on_load<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
    place: PlaceAndQualifiers<'db>,
) -> PlaceAndQualifiers<'db> {
    if place.place.is_definitely_bound() {
        return place;
    }
    let name = attribute.attr.id.as_str();
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
        Some(ty) => Place::bound(ty).into(),
        None => place,
    }
}

/// The write requirement of `object_ty.attribute = ...` in `file`, given the one ty computed.
///
/// Where ty finds no declared member, a Registered property in the file's add-on decides: an
/// instance write must be assignable to the value type, and a write to the class object (the
/// registration itself) is unconstrained. Read-only properties are rejected afterwards by
/// [`check_property_write`].
pub(in crate::types) fn on_store<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &str,
    requirement: AttributeWriteRequirement<'db>,
) -> AttributeWriteRequirement<'db> {
    match requirement {
        AttributeWriteRequirement::Instance {
            object_ty,
            receiver,
            member: InstanceAttributeWriteMember::SetAttr,
        } if let Some(ty) = instance_value_type(db, env, file, object_ty, attribute) => {
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
        AttributeWriteRequirement::Class {
            object_ty,
            member: ClassAttributeWriteMember::Unresolved { .. },
        } if class_receiver(db, object_ty)
            .is_some_and(|class| is_registered_on_class(db, file, class, attribute)) =>
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

/// The Registered properties that complete `receiver.` in `file`, with their value types.
pub(crate) fn completions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
) -> Vec<Member<'db>> {
    let mut members: Vec<Member<'db>> = Vec::new();
    for property in registered_properties(db, file) {
        let name = &property.name;
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
