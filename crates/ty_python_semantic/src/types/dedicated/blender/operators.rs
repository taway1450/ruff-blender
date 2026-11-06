//! Operator calls: `bpy.ops.<category>.<name>(...)` for an add-on's own operators.
//!
//! Blender adds `bpy.ops.my_addon.run` when `register()` registers an operator with the idname
//! `my_addon.run`. No stub can say so, so the attribute is synthesised where ty misses it, from the
//! operators of the add-on of the file that asks ([`addon_operators`]): another add-on's operators
//! stay missing.
//!
//! - A new category (`bpy.ops.my_addon`) is a protocol with one read-only member per operator.
//! - An operator is a protocol whose `__call__` takes the execution context and the undo flag
//!   positionally (or the undo flag alone), then one optional keyword per property, in Blender's
//!   order ([`operator_properties`]), typed as Blender converts it. It also has `poll()`,
//!   `idname()`, `idname_py()` and `get_rna_type()`.
//! - An add-on operator in one of Blender's categories (`bpy.ops.object.my_op`) sits next to the
//!   built-in ones, and replaces a built-in operator with the same idname, as it does at runtime.
//! - A macro takes one dict per step, named by the step's `CATEGORY_OT_name` idname.

use ruff_python_ast::name::Name;
use ty_blender::{EXECUTION_CONTEXTS, OPERATOR_RESULTS, OperatorIdname};
use ty_module_resolver::Module;
use ty_python_core::{ProgramFile, place_table};

use crate::types::callable::CallableTypeKind;
use crate::types::list_members::Member;
use crate::types::signatures::CallableSignature;
use crate::types::typed_dict::{TypedDictFieldBuilder, TypedDictSchema};
use crate::types::{
    CallableType, ClassBase, KnownClass, Parameter, Parameters, Signature, StaticClassLiteral,
    Type, TypedDictType, UnionType,
};
use crate::{Db, ProgramEnvironment};

use super::evaluate::PropertyValue;
use super::index::{AddonInProgram, addon_in_program, addon_index, file_facts};
use super::member::{annotated_property, decorator_properties};
use super::operator_properties::builtin_operator;
use super::properties::{PointerTarget, PropertyDeclaration, PropertyKind};
use super::registration_checks::operator_idname;
use super::symbols::{blender_module_name, bpy_types_bases, is_bpy_types_class};
use super::vector::module_class;
use super::walk::{Event, register_walk};

/// The operators an add-on registers, by idname.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct AddonOperators<'db> {
    pub(super) operators: Box<[AddonOperator<'db>]>,
    /// Whether `register()` was followed to its end. When it was not, every operator class of the
    /// add-on counts, and a macro's steps are unknown.
    pub(super) complete: bool,
}

impl<'db> AddonOperators<'db> {
    fn in_category<'a>(
        &'a self,
        category: &'a str,
    ) -> impl Iterator<Item = &'a AddonOperator<'db>> {
        self.operators
            .iter()
            .filter(move |operator| operator.category == category)
    }

    pub(super) fn get(&self, category: &str, name: &str) -> Option<&AddonOperator<'db>> {
        self.operators
            .iter()
            .find(|operator| operator.category == category && operator.name == name)
    }

    fn categories(&self) -> impl Iterator<Item = &Name> {
        let mut seen: Vec<&Name> = Vec::new();
        self.operators.iter().filter_map(move |operator| {
            (!seen.contains(&&operator.category)).then(|| {
                seen.push(&operator.category);
                &operator.category
            })
        })
    }
}

/// One operator of an add-on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct AddonOperator<'db> {
    pub(super) category: Name,
    pub(super) name: Name,
    pub(super) class: StaticClassLiteral<'db>,
    /// A macro's steps in the order `define()` added them, each in its `CATEGORY_OT_name` form.
    pub(super) steps: Box<[Name]>,
}

impl AddonOperator<'_> {
    pub(super) fn idname(&self) -> OperatorIdname {
        OperatorIdname {
            category: self.category.to_string(),
            name: self.name.to_string(),
        }
    }
}

/// Whether `class` is an operator or a macro of an add-on.
pub(super) fn is_operator_class<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    !is_bpy_types_class(db, class)
        && bpy_types_bases(db, class).any(|base| matches!(base, "Operator" | "Macro"))
}

pub(super) fn is_macro_class<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    bpy_types_bases(db, class).any(|base| base == "Macro")
}

/// The operators of an add-on: the operator classes `register()` registers, or every operator
/// class of the add-on's files when the walk gave up, each under its idname when Blender accepts
/// it. A class registered later with the same idname replaces the earlier one, as at runtime.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| AddonOperators::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn addon_operators<'db>(
    db: &'db dyn Db,
    key: AddonInProgram<'db>,
) -> AddonOperators<'db> {
    let index = addon_index(db, key);
    let mut classes: Vec<StaticClassLiteral<'db>> = index.registered_classes.to_vec();
    if !index.complete {
        for file in key.files(db) {
            for class in &file_facts(db, file).classes {
                if !classes.contains(&class.class) {
                    classes.push(class.class);
                }
            }
        }
    }
    let walk = register_walk(db, key);
    let mut operators: Vec<AddonOperator<'db>> = Vec::new();
    for class in classes {
        if !is_operator_class(db, class) {
            continue;
        }
        let Some(idname) = operator_idname(db, class) else {
            continue;
        };
        let mut steps: Vec<Name> = Vec::new();
        if is_macro_class(db, class) {
            for event in &walk.events {
                if let Event::DefineMacroStep {
                    macro_class,
                    idname: step,
                } = event
                    && *macro_class == class
                    && let Some(step) = OperatorIdname::from_either(step)
                {
                    let step = Name::new(step.bl_form());
                    if !steps.contains(&step) {
                        steps.push(step);
                    }
                }
            }
        }
        let category = Name::new(&idname.category);
        let name = Name::new(&idname.name);
        operators.retain(|operator| operator.category != category || operator.name != name);
        operators.push(AddonOperator {
            category,
            name,
            class,
            steps: steps.into_boxed_slice(),
        });
    }
    AddonOperators {
        operators: operators.into_boxed_slice(),
        complete: index.complete,
    }
}

/// The operators of the add-on `file` belongs to.
fn operators_of<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Option<&'db AddonOperators<'db>> {
    Some(addon_operators(db, addon_in_program(db, file)?))
}

/// The attribute `name` of the module `module`, read in `file`, when it is an operator category or
/// an operator of `file`'s add-on that the stubs do not have. `bound` says whether ty found the
/// attribute: a category the stubs have stays their module, while an add-on operator replaces a
/// built-in one with the same idname.
pub(super) fn module_attribute<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    module: Module<'db>,
    name: &str,
    bound: bool,
) -> Option<Type<'db>> {
    let module_name = bpy_ops_module(db, env, module)?;
    let operators = operators_of(db, file)?;
    match module_name.strip_prefix("bpy.ops") {
        Some("") if !bound => operators
            .in_category(name)
            .next()
            .map(|_| category_type(db, env, operators, name)),
        Some(category) => {
            let category = category.strip_prefix('.')?;
            let operator = operators.get(category, name)?;
            Some(operator_type(db, env, operators, operator))
        }
        None => None,
    }
}

/// The name of `module` when it is Blender's `bpy.ops` or one of its category modules.
fn bpy_ops_module<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    module: Module<'db>,
) -> Option<&'db str> {
    let file = ProgramFile::new(db, module.file(db)?, env.program(db));
    let name = blender_module_name(db, file)?;
    let rest = name.strip_prefix("bpy.ops")?;
    (rest.is_empty()
        || rest
            .strip_prefix('.')
            .is_some_and(|rest| !rest.contains('.')))
    .then_some(name)
}

/// A category of the add-on that the stubs don't have: one read-only member per operator.
fn category_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    operators: &AddonOperators<'db>,
    category: &str,
) -> Type<'db> {
    let members: Vec<(&str, Type<'db>)> = operators
        .in_category(category)
        .map(|operator| {
            (
                operator.name.as_str(),
                operator_type(db, env, operators, operator),
            )
        })
        .collect();
    Type::protocol_with_readonly_members(db, env, members)
}

/// The type of `bpy.ops.<category>.<name>` for an operator of the add-on.
fn operator_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    operators: &AddonOperators<'db>,
    operator: &AddonOperator<'db>,
) -> Type<'db> {
    let idname = operator.idname();
    let string = |value: &str| Type::string_literal(db, value);
    let literals = |values: &[&str]| {
        UnionType::from_elements(db, env, values.iter().map(|value| string(value)))
    };
    let context = literals(&EXECUTION_CONTEXTS);
    let result = KnownClass::Set.to_specialized_instance(db, env, &[literals(&OPERATOR_RESULTS)]);
    let bool_type = KnownClass::Bool.to_instance(db, env);
    let self_parameter = || Parameter::positional_only(Some(Name::new_static("self")));

    let keywords = call_keywords(db, env, operators, operator);
    let with_context = [
        self_parameter(),
        Parameter::positional_only(Some(Name::new_static("execution_context")))
            .with_annotated_type(context)
            .with_default_type(context),
        Parameter::positional_only(Some(Name::new_static("undo")))
            .with_annotated_type(bool_type)
            .with_default_type(bool_type),
    ];
    let undo_alone = [
        self_parameter(),
        Parameter::positional_only(Some(Name::new_static("undo"))).with_annotated_type(bool_type),
    ];
    let call = CallableType::new(
        db,
        CallableSignature::from_overloads([
            Signature::new(
                Parameters::standard(with_context.into_iter().chain(keywords.iter().cloned())),
                result,
            ),
            Signature::new(
                Parameters::standard(undo_alone.into_iter().chain(keywords)),
                result,
            ),
        ]),
        CallableTypeKind::FunctionLike,
    );
    let method = |parameters: Vec<Parameter<'db>>, returns: Type<'db>| {
        CallableType::function_like(
            db,
            Signature::new(Parameters::standard(parameters), returns),
        )
    };
    let poll = method(
        vec![
            self_parameter(),
            Parameter::positional_only(Some(Name::new_static("execution_context")))
                .with_annotated_type(context)
                .with_default_type(context),
        ],
        bool_type,
    );
    let idname_method = method(vec![self_parameter()], string(&idname.bl_form()));
    let idname_py = method(vec![self_parameter()], string(&idname.py_form()));
    let struct_type = module_class(db, env, "bpy.types", "Struct")
        .map_or_else(Type::any, |class| {
            Type::instance(db, env, class.default_specialization(db))
        });
    let get_rna_type = method(vec![self_parameter()], struct_type);
    Type::protocol_with_methods(
        db,
        env,
        [
            ("__call__", call),
            ("poll", poll),
            ("idname", idname_method),
            ("idname_py", idname_py),
            ("get_rna_type", get_rna_type),
        ],
    )
}

/// The keyword-only parameters of an operator's call: one per property, or for a macro one dict
/// per step.
fn call_keywords<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    operators: &AddonOperators<'db>,
    operator: &AddonOperator<'db>,
) -> Vec<Parameter<'db>> {
    let keyword = |name: Name, ty: Type<'db>| {
        Parameter::keyword_only(name)
            .with_annotated_type(ty)
            .with_default_type(ty)
    };
    if !is_macro_class(db, operator.class) {
        return operator_properties(db, operator.class)
            .iter()
            .map(|property| {
                keyword(
                    property.name.clone(),
                    argument_type(db, env, &property.value, 0),
                )
            })
            .collect();
    }
    if !operators.complete {
        // `define()` may have run where the walk could not follow.
        return vec![
            Parameter::keyword_variadic(Name::new_static("kwargs"))
                .with_annotated_type(Type::any()),
        ];
    }
    operator
        .steps
        .iter()
        .map(|step| {
            let ty = OperatorIdname::from_either(step)
                .and_then(|idname| operators.get(&idname.category, &idname.name))
                .map_or_else(
                    // A built-in step takes the arguments of its `bpy.ops` call.
                    || {
                        OperatorIdname::from_either(step)
                            .and_then(|idname| builtin_operator(db, env, &idname))
                            .map_or_else(
                                || {
                                    KnownClass::Dict.to_specialized_instance(
                                        db,
                                        env,
                                        &[KnownClass::Str.to_instance(db, env), Type::any()],
                                    )
                                },
                                |step| {
                                    typed_dict(
                                        db,
                                        step.properties.into_iter().filter_map(|property| {
                                            Some((property.name, property.write?))
                                        }),
                                        false,
                                    )
                                },
                            )
                    },
                    |step| {
                        typed_dict(
                            db,
                            operator_properties(db, step.class).iter().map(|property| {
                                (
                                    property.name.clone(),
                                    argument_type(db, env, &property.value, 0),
                                )
                            }),
                            false,
                        )
                    },
                );
            keyword(step.clone(), ty)
        })
        .collect()
}

/// A property of a class, in Blender's order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct ClassProperty<'db> {
    pub(super) name: Name,
    pub(super) value: PropertyValue<'db>,
    /// The class that declares it.
    pub(super) owner: StaticClassLiteral<'db>,
}

/// The properties Blender gives `class` when it registers it, in its order.
///
/// Blender reads them with `typing.get_type_hints(cls)`, which merges the annotations of the method
/// resolution order from its end: the base-most class first, every class counting (mixins and
/// registered base operators alike). A name declared again keeps its first position and takes the
/// most derived declaration; an annotation that is not a property removes it. The properties that a
/// decorator adds (`@orientation_helper`) follow the body's annotations of the decorated class.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn class_properties<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Box<[ClassProperty<'db>]> {
    let mro: Vec<StaticClassLiteral<'db>> = class
        .iter_mro(db, None)
        .filter_map(|base| match base {
            ClassBase::Class(class_type) => class_type.class_literal(db).as_static(),
            _ => None,
        })
        .collect();
    let mut properties: Vec<ClassProperty<'db>> = Vec::new();
    for base in mro.into_iter().rev() {
        let table = place_table(db, base.body_scope(db));
        let declared = table
            .symbols()
            .filter(|symbol| symbol.is_declared())
            .map(|symbol| symbol.name().clone());
        let decorated = decorator_properties(db, base);
        let names: Vec<(Name, Option<PropertyValue<'db>>)> = declared
            .map(|name| {
                let value = annotated_property(db, base, &name).cloned();
                (name, value)
            })
            .chain(decorated)
            .collect();
        for (name, value) in names {
            let position = properties.iter().position(|property| property.name == name);
            match (value, position) {
                (Some(value), Some(position)) => {
                    properties[position].value = value;
                    properties[position].owner = base;
                }
                (Some(value), None) => properties.push(ClassProperty {
                    name,
                    value,
                    owner: base,
                }),
                (None, Some(position)) => {
                    properties.remove(position);
                }
                (None, None) => {}
            }
        }
    }
    properties.into_boxed_slice()
}

/// The properties an operator's call takes: [`class_properties`] up to the first that Blender
/// refuses on an operator ([`refused_property`]), which it drops with every property after it.
pub(super) fn operator_properties<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> &'db [ClassProperty<'db>] {
    let properties = class_properties(db, class);
    let end = refused_property(db, class).unwrap_or(properties.len());
    &properties[..end]
}

/// The position in [`class_properties`] of the first property Blender refuses on the operator
/// `class`: a pointer to a data-block (an ID), or a pointer or collection to a group that contains
/// one at any depth (`STRUCT_CONTAINS_DATABLOCK_IDPROPERTIES`, `bpy_props.cc`).
pub(super) fn refused_property<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<usize> {
    class_properties(db, class)
        .iter()
        .position(|property| contains_data_blocks(db, &property.value, 0))
}

/// How deep groups in groups are followed.
const MAX_GROUP_DEPTH: u32 = 8;

/// Whether a property is, or holds, a pointer to a data-block.
pub(super) fn contains_data_blocks<'db>(
    db: &'db dyn Db,
    value: &PropertyValue<'db>,
    depth: u32,
) -> bool {
    let PropertyValue::Declarations(declarations) = value else {
        return false;
    };
    declarations
        .iter()
        .any(|declaration| declaration_contains_data_blocks(db, declaration, depth).is_some())
}

/// The class that makes a declaration hold data-blocks: the ID class it points to, or the group it
/// points to when that group holds one at any depth. `None` when it holds none.
pub(super) fn declaration_contains_data_blocks<'db>(
    db: &'db dyn Db,
    declaration: &PropertyDeclaration<'db>,
    depth: u32,
) -> Option<StaticClassLiteral<'db>> {
    if !matches!(
        declaration.kind,
        PropertyKind::Pointer | PropertyKind::Collection
    ) {
        return None;
    }
    match declaration.target_class(db)? {
        (id, PointerTarget::Id) => Some(id),
        (group, PointerTarget::PropertyGroup) => (depth < MAX_GROUP_DEPTH
            && class_properties(db, group)
                .iter()
                .any(|property| contains_data_blocks(db, &property.value, depth + 1)))
        .then_some(group),
    }
}

/// The type an operator call takes for a property, as Blender converts it.
fn argument_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value: &PropertyValue<'db>,
    depth: u32,
) -> Type<'db> {
    match value {
        PropertyValue::Declarations(declarations) => UnionType::from_elements(
            db,
            env,
            declarations
                .iter()
                .map(|declaration| declaration_argument_type(db, env, declaration, depth)),
        ),
        PropertyValue::Opaque => Type::any(),
    }
}

/// The argument type of one declaration: the write type, except that a pointer to a group takes a
/// dict of the group's properties, all optional, and a collection a sequence of dicts with every
/// property of the item (Blender raises `TypeError` for a missing one).
fn declaration_argument_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    declaration: &PropertyDeclaration<'db>,
    depth: u32,
) -> Type<'db> {
    let group = || match declaration.target_class(db) {
        Some((group, PointerTarget::PropertyGroup)) if depth < MAX_GROUP_DEPTH => Some(group),
        _ => None,
    };
    let group_dict = |group: StaticClassLiteral<'db>, required: bool| {
        let properties = class_properties(db, group);
        // Every group has `name`, an RNA property of `PropertyGroup` rather than an annotation.
        let named = (!properties.iter().any(|property| property.name == "name")).then(|| {
            (
                Name::new_static("name"),
                KnownClass::Str.to_instance(db, env),
            )
        });
        typed_dict(
            db,
            named.into_iter().chain(properties.iter().map(|property| {
                (
                    property.name.clone(),
                    argument_type(db, env, &property.value, depth + 1),
                )
            })),
            required,
        )
    };
    match declaration.kind {
        PropertyKind::Pointer => group().map_or_else(Type::any, |group| group_dict(group, false)),
        PropertyKind::Collection => group().map_or_else(Type::any, |group| {
            KnownClass::Sequence.to_specialized_instance(db, env, &[group_dict(group, true)])
        }),
        _ => declaration.write_type(db, env),
    }
}

/// A `TypedDict` with one key per `(name, type)` of `fields`, all required or all optional.
fn typed_dict<'db>(
    db: &'db dyn Db,
    fields: impl Iterator<Item = (Name, Type<'db>)>,
    required: bool,
) -> Type<'db> {
    let schema: TypedDictSchema<'db> = fields
        .map(|(name, ty)| {
            (
                name,
                TypedDictFieldBuilder::new(ty).required(required).build(),
            )
        })
        .collect();
    Type::TypedDict(TypedDictType::from_schema_items(db, schema))
}

/// The categories and operators of `file`'s add-on that complete `receiver.`: the new categories
/// under `bpy.ops`, the add-on's operators under a category module or a new category.
pub(super) fn completions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
) -> Vec<Member<'db>> {
    let Some(operators) = operators_of(db, file) else {
        return Vec::new();
    };
    let member = |name: &Name, ty: Type<'db>| Member {
        name: name.clone(),
        ty,
        is_type_check_only: false,
    };
    let operator_members = |category: &str| {
        operators
            .in_category(category)
            .map(|operator| member(&operator.name, operator_type(db, env, operators, operator)))
            .collect::<Vec<_>>()
    };
    if let Type::ModuleLiteral(module) = receiver
        && let Some(name) = bpy_ops_module(db, env, module.module(db))
    {
        return match name.strip_prefix("bpy.ops.") {
            Some(category) => operator_members(category),
            None => operators
                .categories()
                .filter(|category| {
                    !receiver
                        .member(db, env, category)
                        .place
                        .is_definitely_bound()
                })
                .map(|category| member(category, category_type(db, env, operators, category)))
                .collect(),
        };
    }
    if let Type::ProtocolInstance(_) = receiver {
        for category in operators.categories() {
            if category_type(db, env, operators, category) == receiver {
                return operator_members(category);
            }
        }
    }
    Vec::new()
}
