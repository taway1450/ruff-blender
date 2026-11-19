//! Constructor narrowing: a constructor whose argument is a Type tag value returns the subclass the
//! tag names, since Blender refines the struct it creates like any other.
//!
//! ```python
//! ob.constraints.new('COPY_LOCATION')     # `CopyLocationConstraint`
//! bpy.data.lights.new("Key", 'SPOT')      # `SpotLight`
//! tree.nodes.new(type='ShaderNodeMath')   # `ShaderNodeMath`
//! self.gizmos.new(MyGizmo.bl_idname)      # `MyGizmo`
//! tree.interface.new_socket("Fac", socket_type='NodeSocketFloat')  # `NodeTreeInterfaceSocketFloat`
//! ```
//!
//! Blender's own stubs type the parameter with the same `rna_enum_*` alias as the tag of the struct
//! it returns (`ObjectConstraints.new(type: rna_enum_constraint_type_items) -> Constraint`, and
//! `Constraint.type: rna_enum_constraint_type_items`), so the Type tag table gives the class for
//! the argument's value. `Nodes.new` and `Gizmos.new` take a `bl_idname` typed `str`
//! ([`ty_blender::idname_constructor`]): a built-in value is looked up in the table, an add-on's in
//! its classes, and `X.bl_idname` names `X`. The narrowing is silent: Blender's C code makes it.
//! A non-literal argument, or a value no class declares, keeps the stub's return type.
//!
//! `bpy.data.objects.new(name, data)` takes the object's type from its data
//! (`BKE_object_obdata_to_type`), so it returns the Tag view of that type: a `Camera` gives an
//! `_Object_CAMERA`, `None` an `_Object_EMPTY` ([`by_object_data`]).

use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Expr};
use ty_blender::{IdnameConstructor, idname_constructor, object_types_of_data};

use crate::types::call::Bindings;
use crate::types::context::InferContext;
use crate::types::{StaticClassLiteral, Type, UnionType};
use crate::{Db, ProgramEnvironment};

use super::classes::{class_string, maybe_registered_classes, mro_classes};
use super::index::addon_in_program;
use super::operator_properties::receiver_class;
use super::symbols::{blender_module_name, bpy_types_bases, is_bpy_types_class};
use super::type_tags::{instance, narrow_to_tag_values, string_literals, tag_attributes};
use super::vector::module_class;

/// Narrows the return type of each matching overload of a `bpy.types` method in `bindings` by its
/// Type tag argument.
pub(super) fn on_call<'db>(
    context: &InferContext<'db, '_>,
    call: &ast::ExprCall,
    bindings: &mut Bindings<'db>,
    expression_type: &dyn Fn(&Expr) -> Option<Type<'db>>,
) {
    let db = context.db();
    let env = context.program_environment();
    for callable in bindings.iter_flat_mut() {
        let Type::BoundMethod(method) = callable.callable_type else {
            continue;
        };
        let Some(function) = method.function(db) else {
            continue;
        };
        if blender_module_name(db, function.program_file(db)) != Some("bpy.types") {
            continue;
        }
        let method_name = function.name(db).as_str();
        let receiver = receiver_class(db, env, method.self_instance(db));
        let idname = receiver.and_then(|receiver| {
            bpy_types_bases(db, receiver).find_map(|base| idname_constructor(base, method_name))
        });
        let creates_object = method_name == "new"
            && receiver.is_some_and(|receiver| {
                bpy_types_bases(db, receiver).any(|base| base == "BlendDataObjects")
            });
        for (_, binding) in callable.matching_overloads_mut() {
            let parameters = binding.signature.parameters();
            let arguments: Vec<(Type<'db>, Type<'db>, Option<&Name>)> = parameters
                .iter()
                .zip(binding.parameter_types())
                .filter_map(|(parameter, argument)| {
                    Some((parameter.annotated_type(), (*argument)?, parameter.name()))
                })
                .collect();
            if creates_object {
                let data = arguments
                    .iter()
                    .find(|(_, _, name)| name.is_some_and(|name| name == "object_data"))
                    .map(|(_, argument, _)| *argument);
                let narrowed = data.and_then(|data| {
                    map_structs(db, env, binding.return_ty, |base| {
                        by_object_data(db, env, base, data)
                    })
                });
                if let Some(narrowed) = narrowed {
                    binding.set_return_type(narrowed);
                }
                continue;
            }
            let narrowed = map_structs(db, env, binding.return_ty, |base| match idname {
                Some(constructor) => {
                    let argument = arguments
                        .iter()
                        .find(|(_, _, name)| name.is_some_and(|name| name == constructor.parameter))
                        .map(|(_, argument, _)| *argument)?;
                    by_idname(
                        context,
                        constructor,
                        base,
                        argument,
                        call.arguments
                            .find_argument_value(constructor.parameter, constructor.position),
                        expression_type,
                    )
                }
                None => arguments.iter().find_map(|(annotated, argument, _)| {
                    by_tag_alias(db, env, base, *annotated, *argument)
                }),
            });
            if let Some(narrowed) = narrowed {
                binding.set_return_type(narrowed);
            }
        }
    }
}

/// `ty` with each instance of a `bpy.types` class in it replaced by what `narrow` gives for the
/// class, `None` and the other elements of a union kept. `None` when `narrow` gives nothing.
fn map_structs<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    narrow: impl Fn(StaticClassLiteral<'db>) -> Option<Type<'db>>,
) -> Option<Type<'db>> {
    let narrow_element = |element: Type<'db>| {
        let Type::NominalInstance(instance) = element else {
            return None;
        };
        let class = instance.class(db, env).class_literal(db).as_static()?;
        if !is_bpy_types_class(db, class) {
            return None;
        }
        narrow(class)
    };
    match ty {
        Type::Union(union) => {
            let mut changed = false;
            let elements: Vec<Type<'db>> = union
                .elements(db)
                .iter()
                .map(|element| {
                    narrow_element(*element)
                        .inspect(|_| changed = true)
                        .unwrap_or(*element)
                })
                .collect();
            changed.then(|| UnionType::from_elements(db, env, elements))
        }
        _ => narrow_element(ty),
    }
}

/// The object `bpy.data.objects.new(name, data)` creates, a `base` (`Object`), from the type of its
/// `data`: the Tag view of the object type each element names ([`object_types_of_data`]), `None`
/// giving an `'EMPTY'`. `None` when an element is no known object data (`ID`).
fn by_object_data<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    base: StaticClassLiteral<'db>,
    data: Type<'db>,
) -> Option<Type<'db>> {
    let elements = match data {
        Type::Union(union) => union.elements(db).to_vec(),
        _ => vec![data],
    };
    let mut values: Vec<Name> = Vec::new();
    for element in elements {
        let types: &[&str] = if element.is_none(db) {
            &["EMPTY"]
        } else {
            let Type::NominalInstance(instance) = element else {
                return None;
            };
            let class = instance.class(db, env).class_literal(db).as_static()?;
            bpy_types_bases(db, class).find_map(object_types_of_data)?
        };
        for value in types {
            let value = Name::new_static(value);
            if !values.contains(&value) {
                values.push(value);
            }
        }
    }
    narrow_to_tag_values(db, env, instance(db, env, base), "type", &values)
}

/// The struct a constructor returning a `base` creates, when the parameter's `annotated` type is
/// that of one of `base`'s Type tags (the same `rna_enum_*` alias) and the `argument` is a string
/// literal, or a union of them, every one naming a class.
fn by_tag_alias<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    base: StaticClassLiteral<'db>,
    annotated: Type<'db>,
    argument: Type<'db>,
) -> Option<Type<'db>> {
    let base_ty = instance(db, env, base);
    let attribute = tag_attributes(db, base).iter().find(|attribute| {
        base_ty
            .member(db, env, attribute)
            .place
            .ignore_possibly_undefined()
            .is_some_and(|tag_ty| annotated.is_equivalent_to(db, env, tag_ty))
    })?;
    let values = string_literals(db, argument)?;
    narrow_to_tag_values(db, env, base_ty, attribute, &values)
}

/// The struct `nodes.new(...)` or `gizmos.new(...)` creates from the `bl_idname` `argument`: for
/// each of its literal values the stub class the Type tag table names or the add-on's class with
/// that `bl_idname`, or `X` for `X.bl_idname`. `None` when a value names no class.
fn by_idname<'db>(
    context: &InferContext<'db, '_>,
    constructor: IdnameConstructor,
    base: StaticClassLiteral<'db>,
    argument: Type<'db>,
    argument_expression: Option<&Expr>,
    expression_type: &dyn Fn(&Expr) -> Option<Type<'db>>,
) -> Option<Type<'db>> {
    let db = context.db();
    let env = context.program_environment();
    let base_ty = instance(db, env, base);
    let derives_from_base = |class: StaticClassLiteral<'db>| {
        class != base && mro_classes(db, class).any(|ancestor| ancestor == base)
    };
    if let Some(values) = string_literals(db, argument) {
        if let Some((idname_prefix, class_prefix)) = constructor.class_prefix {
            // A built-in idname's paired struct; an add-on's idname keeps the base.
            let classes = values
                .iter()
                .map(|value| {
                    let rest = value.strip_prefix(idname_prefix)?;
                    module_class(db, env, "bpy.types", &format!("{class_prefix}{rest}"))
                        .filter(|class| derives_from_base(*class))
                        .map(|class| instance(db, env, class))
                })
                .collect::<Option<Vec<_>>>()?;
            return Some(UnionType::from_elements(db, env, classes));
        }
        let classes = values
            .iter()
            .map(|value| {
                narrow_to_tag_values(
                    db,
                    env,
                    base_ty,
                    constructor.attribute,
                    std::slice::from_ref(value),
                )
                .or_else(|| {
                    let key = addon_in_program(db, context.program_file())?;
                    maybe_registered_classes(db, key)
                        .into_iter()
                        .find(|class| {
                            derives_from_base(*class)
                                && class_string(db, *class, constructor.attribute).as_deref()
                                    == Some(value.as_str())
                        })
                        .map(|class| instance(db, env, class))
                })
            })
            .collect::<Option<Vec<_>>>()?;
        return Some(UnionType::from_elements(db, env, classes));
    }
    // `MyGizmo.bl_idname`, which ty types as `str`: the declaration in `bpy.types` wins over the
    // add-on's literal.
    let Some(Expr::Attribute(attribute)) = argument_expression else {
        return None;
    };
    if attribute.attr.as_str() != constructor.attribute || constructor.class_prefix.is_some() {
        return None;
    }
    let Some(Type::ClassLiteral(class)) = expression_type(&attribute.value) else {
        return None;
    };
    let class = class.as_static()?;
    derives_from_base(class).then(|| instance(db, env, class))
}
