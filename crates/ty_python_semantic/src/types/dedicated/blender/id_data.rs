//! The data-block behind `id_data`: the ID that stores a struct.
//!
//! The stubs give every struct the `id_data` of its kind (`PoseBone.id_data` is an `Object`), and
//! `bpy_struct.id_data` an `ID` otherwise. Two cases know better:
//!
//! - a data-block is its own `id_data`: `obj.id_data` is `obj`;
//! - an add-on's `PropertyGroup` lives in the data-blocks that its properties are registered on:
//!   after `bpy.types.Armature.slots = CollectionProperty(type=Slot)`, a `Slot`'s `id_data` is an
//!   `Armature`.
//!
//! A group's owners are the classes with a pointer or collection property to it (a Registered
//! property, or an Annotated property of a registered class or one of its mixins). An owner that is
//! an ID gives itself; an owner that is another group gives that group's `id_data`, so a group nested
//! in a group on `Scene` is in a `Scene`; any other owner (a `PoseBone`, a `Node`) gives its own
//! `id_data`. A property to a base of the group does not store the group, but one to a subclass
//! does: a method of the base may run on an instance of the subclass. With no known owner, an
//! owner whose `id_data` is unknown, or a property the checker cannot read (which may store the
//! group), the stubs' type stays.

use ty_python_core::place_table;

use crate::types::{StaticClassLiteral, Type, TypeVarBoundOrConstraints, UnionType};
use crate::{Db, ProgramEnvironment};

use super::classes::{derives_from, maybe_registered_classes, mro_classes};
use super::evaluate::PropertyValue;
use super::index::{addon_in_program, file_facts, registered_properties};
use super::member::{annotated_property, is_ignored_host};
use super::properties::{Known, PropertyKind};
use super::symbols::{bpy_types_bases, is_bpy_types_class};

/// The type of `receiver.id_data`, when it is known better than the stubs say.
///
/// Every arm of a union must be known; a bounded type variable reads its bound but keeps itself
/// when that is a data-block (`Self` in a method of an `Object` subclass).
pub(super) fn id_data_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    receiver: Type<'db>,
) -> Option<Type<'db>> {
    match receiver {
        Type::NominalInstance(instance) => {
            let class = instance.class(db, env).class_literal(db).as_static()?;
            if is_data_block(db, class) {
                Some(receiver)
            } else if derives_from(db, class, &["PropertyGroup"]) {
                owners_id_data(db, env, class, &mut Vec::new())
            } else {
                None
            }
        }
        Type::TypeVar(typevar) => match typevar.typevar(db).bound_or_constraints(db, env)? {
            TypeVarBoundOrConstraints::UpperBound(bound) => {
                let bound_type = id_data_type(db, env, bound)?;
                Some(if bound_type == bound {
                    receiver
                } else {
                    bound_type
                })
            }
            TypeVarBoundOrConstraints::Constraints(_) => None,
        },
        Type::Union(union) => {
            let arms = union
                .elements(db)
                .iter()
                .map(|arm| id_data_type(db, env, *arm))
                .collect::<Option<Vec<_>>>()?;
            Some(UnionType::from_elements(db, env, arms))
        }
        _ => None,
    }
}

/// Whether `class` is an ID class, Blender's or an add-on's.
fn is_data_block<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    bpy_types_bases(db, class).any(|base| base == "ID")
}

/// The union of the `id_data` of the owners of the add-on's `PropertyGroup` class `group`, or
/// `None` when one is unknown. `visiting` holds the groups whose owners are being resolved: a
/// group reached again adds nothing.
fn owners_id_data<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    group: StaticClassLiteral<'db>,
    visiting: &mut Vec<StaticClassLiteral<'db>>,
) -> Option<Type<'db>> {
    if visiting.contains(&group) {
        return Some(Type::Never);
    }
    let owners = group_owners(db, group).as_deref()?;
    if owners.is_empty() {
        return None;
    }
    visiting.push(group);
    let mut types = Vec::with_capacity(owners.len());
    for owner in owners {
        let instance = Type::instance(db, env, owner.default_specialization(db));
        types.push(if is_data_block(db, *owner) {
            instance
        } else if derives_from(db, *owner, &["PropertyGroup"]) {
            owners_id_data(db, env, *owner, visiting)?
        } else {
            instance
                .member(db, env, "id_data")
                .place
                .ignore_possibly_undefined()?
        });
    }
    visiting.pop();
    let union = UnionType::from_elements(db, env, types);
    (!union.is_never()).then_some(union)
}

/// The classes that store instances of the add-on's `PropertyGroup` class `group`, in the order
/// they are found: the owners of its Registered properties, then the registered classes whose
/// annotations (or a mixin's) declare one. `None` when a property may store it but the checker
/// cannot tell: a pointer or collection whose `type=` it cannot read (see [`target_stores`]), or a
/// property it cannot read at all. One such property anywhere in the add-on may store any group.
///
/// Reading a property's value infers the class its `type=` names, which may read the owners again;
/// such a cycle starts from no owners.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| Some(Box::default()),
    heap_size=ruff_memory_usage::heap_size
)]
fn group_owners<'db>(
    db: &'db dyn Db,
    group: StaticClassLiteral<'db>,
) -> Option<Box<[StaticClassLiteral<'db>]>> {
    let file = group.program_file(db);
    // Whether the property `value` stores `group`, or `None` when that is unknown.
    let stores = |value: &PropertyValue<'db>| match value {
        PropertyValue::Declarations(declarations) => {
            declarations.iter().try_fold(false, |stores, declaration| {
                if !matches!(
                    declaration.kind,
                    PropertyKind::Pointer | PropertyKind::Collection
                ) {
                    return Some(stores);
                }
                let Known::Value(target) = declaration.target else {
                    return None;
                };
                Some(stores || target_stores(db, target, group)?)
            })
        }
        PropertyValue::Opaque => None,
    };
    let mut owners: Vec<StaticClassLiteral<'db>> = Vec::new();
    for property in registered_properties(db, file) {
        if property.accepted && stores(&property.value)? && !owners.contains(&property.owner) {
            owners.push(property.owner);
        }
    }
    let hosts = match addon_in_program(db, file) {
        Some(key) => maybe_registered_classes(db, key),
        None => file_facts(db, file)
            .classes
            .iter()
            .map(|class| class.class)
            .collect(),
    };
    for host in hosts {
        if owners.contains(&host) || is_ignored_host(db, host) {
            continue;
        }
        let mut declares = false;
        for class in mro_classes(db, host).filter(|class| !is_bpy_types_class(db, *class)) {
            for symbol in place_table(db, class.body_scope(db)).symbols() {
                if let Some(value) = annotated_property(db, class, symbol.name()) {
                    declares |= stores(value)?;
                }
            }
        }
        if declares {
            owners.push(host);
        }
    }
    Some(owners.into_boxed_slice())
}

/// Whether a pointer or collection whose `type=` is `target` may store an instance of `group`, or
/// `None` when the checker cannot tell.
///
/// `type=Group` stores `Group` and its subclasses, so a property to a subclass of `group` does too;
/// a union may store `group` through any of its arms. Any other type (a call the checker cannot
/// infer, a `type[...]`) may name `group`.
fn target_stores<'db>(
    db: &'db dyn Db,
    target: Type<'db>,
    group: StaticClassLiteral<'db>,
) -> Option<bool> {
    match target {
        Type::ClassLiteral(class) => {
            Some(mro_classes(db, class.as_static()?).any(|base| base == group))
        }
        Type::Union(union) => union.elements(db).iter().try_fold(false, |stores, arm| {
            Some(stores || target_stores(db, *arm, group)?)
        }),
        _ => None,
    }
}
