//! Property declarations: `count: IntProperty(min=0)` on a Blender class.
//!
//! Blender turns an annotation that is a call to a `bpy.props` function into a property, and the
//! instance attribute then holds the property's value (an `int` here), not the `_PropertyDeferred`
//! the stubs return. The annotation takeover in type-expression inference asks this module for that
//! value type, and the class-member lookups ask [`property_declaration`] whether a member is such a
//! declaration.

use ruff_db::parsed::{parsed_module, parsed_string_annotation};
use ruff_db::source::source_text;
use ruff_python_ast::helpers::any_over_expr;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Expr};
use ty_module_resolver::{ModuleName, resolve_module_confident};
use ty_python_core::definition::{Definition, DefinitionKind, DefinitionState};
use ty_python_core::scope::{ScopeId, ScopeKind};
use ty_python_core::{ProgramFile, place_table, use_def_map};

use crate::place::imported_symbol;
use crate::types::context::InferContext;
use crate::types::definition_resolution::{ImportAliasResolution, definitions_for_name};
use crate::types::diagnostic::INVALID_ASSIGNMENT;
use crate::types::infer::infer_definition_types;
use crate::types::{
    ClassBase, ClassType, KnownClass, KnownUnion, StaticClassLiteral, Type, UnionType,
    definition_expression_type_in_scope,
};
use crate::{Db, ProgramEnvironment};

use super::registered::is_registered_read_only;
use super::symbols::{blender_function_name, bpy_types_bases};

/// The ten functions of `bpy.props` that declare a property.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum PropertyKind {
    Bool,
    BoolVector,
    Collection,
    Enum,
    Float,
    FloatVector,
    Int,
    IntVector,
    Pointer,
    String,
}

impl PropertyKind {
    const ALL: [(&'static str, PropertyKind); 10] = [
        ("BoolProperty", PropertyKind::Bool),
        ("BoolVectorProperty", PropertyKind::BoolVector),
        ("CollectionProperty", PropertyKind::Collection),
        ("EnumProperty", PropertyKind::Enum),
        ("FloatProperty", PropertyKind::Float),
        ("FloatVectorProperty", PropertyKind::FloatVector),
        ("IntProperty", PropertyKind::Int),
        ("IntVectorProperty", PropertyKind::IntVector),
        ("PointerProperty", PropertyKind::Pointer),
        ("StringProperty", PropertyKind::String),
    ];

    /// The kind of property that a call to `callee` declares, when `callee` is one of the ten
    /// `bpy.props` functions, however it was imported or aliased.
    fn of_callee<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<Self> {
        let name = blender_function_name(db, callee, "bpy.props")?;
        Self::ALL
            .iter()
            .find_map(|(function, kind)| (*function == name).then_some(*kind))
    }
}

/// What a property declaration says about the property Blender creates from it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct PropertyDeclaration<'db> {
    kind: PropertyKind,
    /// The identifiers of an enum's static `items=`, or `None` when they are computed.
    enum_items: Option<Box<[Name]>>,
    /// `options={"ENUM_FLAG"}`: the value is a set of identifiers.
    enum_flag: bool,
    /// `options={"READ_ONLY"}`: the property cannot be written.
    read_only_option: bool,
    /// `StringProperty(subtype="BYTE_STRING")`: the value is `bytes`.
    byte_string: bool,
    /// The inferred type of a pointer's or collection's `type=`, or `None` when it is missing.
    target: Option<Type<'db>>,
}

impl<'db> PropertyDeclaration<'db> {
    /// Describe the property that `call` declares, or `None` when `callee` (the inferred type of
    /// the call's function) is not one of the ten `bpy.props` functions.
    ///
    /// `items=`, `options=` and `subtype=` are read from the syntax, because ty widens list and set
    /// literals to `list[tuple[str, str, str]]` and `set[str]`. Only keyword arguments are read, as
    /// Blender only accepts them for these parameters. `scope` is where a module constant passed as
    /// `items=` is looked up. `type=` is read from its inferred type, which `argument_type` gives,
    /// so that aliases and forward references to the target class work.
    pub(super) fn from_call(
        db: &'db dyn Db,
        scope: ScopeId<'db>,
        callee: Type<'db>,
        call: &ast::ExprCall,
        argument_type: impl Fn(&Expr) -> Type<'db>,
    ) -> Option<Self> {
        let kind = PropertyKind::of_callee(db, callee)?;
        let keyword = |name: &str| call.arguments.find_keyword(name).map(|kw| &kw.value);
        let options = keyword("options").map(string_set).unwrap_or_default();

        Some(PropertyDeclaration {
            kind,
            enum_items: (kind == PropertyKind::Enum)
                .then(|| keyword("items").and_then(|items| static_enum_items(db, scope, items)))
                .flatten(),
            enum_flag: options.iter().any(|option| option == "ENUM_FLAG"),
            read_only_option: options.iter().any(|option| option == "READ_ONLY"),
            byte_string: keyword("subtype")
                .and_then(Expr::as_string_literal_expr)
                .is_some_and(|subtype| subtype.value.to_str() == "BYTE_STRING"),
            target: matches!(kind, PropertyKind::Pointer | PropertyKind::Collection)
                .then(|| keyword("type").map(argument_type))
                .flatten(),
        })
    }

    /// The type of the attribute Blender creates from this declaration.
    pub(super) fn value_type(&self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        match self.kind {
            PropertyKind::Bool => KnownClass::Bool.to_instance(db, env),
            PropertyKind::Int => KnownClass::Int.to_instance(db, env),
            // What the annotation `float` means, so that `assert_type(x, float)` holds.
            PropertyKind::Float => KnownUnion::Float.to_type(db, env),
            PropertyKind::String if self.byte_string => KnownClass::Bytes.to_instance(db, env),
            PropertyKind::String => KnownClass::Str.to_instance(db, env),
            PropertyKind::Enum => {
                let identifier = match self.enum_items.as_deref() {
                    Some(items) if !items.is_empty() => UnionType::from_elements(
                        db,
                        env,
                        items
                            .iter()
                            .map(|item| Type::string_literal(db, item.as_str())),
                    ),
                    _ => KnownClass::Str.to_instance(db, env),
                };
                if self.enum_flag {
                    KnownClass::Set.to_specialized_instance(db, env, &[identifier])
                } else {
                    identifier
                }
            }
            PropertyKind::Pointer => match self.target_class(db) {
                Some((class, PointerTarget::PropertyGroup)) => {
                    Type::instance(db, env, class.default_specialization(db))
                }
                Some((class, PointerTarget::Id)) => UnionType::from_elements(
                    db,
                    env,
                    [
                        Type::instance(db, env, class.default_specialization(db)),
                        Type::none(db, env),
                    ],
                ),
                None => Type::unknown(),
            },
            PropertyKind::Collection => {
                let Some((item, _)) = self.target_class(db) else {
                    return Type::unknown();
                };
                let item = Type::instance(db, env, item.default_specialization(db));
                bpy_types_class(db, env, "bpy_prop_collection_idprop").map_or_else(
                    Type::unknown,
                    // Only a generic stub class can say what the collection holds; a
                    // non-generic one is used as it is.
                    |collection| {
                        let collection = collection.apply_specialization(db, |generic_context| {
                            generic_context.specialize(db, vec![item])
                        });
                        Type::instance(db, env, collection)
                    },
                )
            }
            PropertyKind::BoolVector | PropertyKind::FloatVector | PropertyKind::IntVector => {
                Type::unknown()
            }
        }
    }

    /// Whether writing the attribute is an error: a collection, a pointer to a `PropertyGroup`
    /// (Blender creates the group with its owner), or any property with `options={"READ_ONLY"}`.
    pub(super) fn is_read_only(&self, db: &'db dyn Db) -> bool {
        self.read_only_option
            || match self.kind {
                PropertyKind::Collection => true,
                PropertyKind::Pointer => matches!(
                    self.target_class(db),
                    Some((_, PointerTarget::PropertyGroup))
                ),
                _ => false,
            }
    }

    /// The class `type=` names, when it is an ID or a `PropertyGroup` class.
    fn target_class(&self, db: &'db dyn Db) -> Option<(StaticClassLiteral<'db>, PointerTarget)> {
        let Type::ClassLiteral(class) = self.target? else {
            return None;
        };
        let class = class.as_static()?;
        bpy_types_bases(db, class).find_map(|base| match base {
            "ID" => Some((class, PointerTarget::Id)),
            "PropertyGroup" => Some((class, PointerTarget::PropertyGroup)),
            _ => None,
        })
    }
}

/// What a pointer property points to: an ID (a data-block, which may be unset) or a
/// `PropertyGroup` (which Blender creates along with its owner).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PointerTarget {
    Id,
    PropertyGroup,
}

/// The class `name` of `bpy.types`, as the stubs define it.
fn bpy_types_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    name: &str,
) -> Option<StaticClassLiteral<'db>> {
    let module = resolve_module_confident(
        db,
        env.resolver_environment(db),
        &ModuleName::new_static("bpy.types")?,
    )?;
    let file = ProgramFile::new(db, module.file(db)?, env.program(db));
    let Type::ClassLiteral(class) = imported_symbol(db, env, Some(file), name, None)
        .place
        .ignore_possibly_undefined()?
    else {
        return None;
    };
    class.as_static()
}

/// Whether the call in a quoted annotation may declare a property and should be inferred.
///
/// Calls in quoted annotations are not inferred by ty, because the parsed string is not part of the
/// semantic index. This syntactic pre-filter lets through only calls whose function is spelled like
/// one of the ten `bpy.props` functions (`IntProperty`, `props.IntProperty`, `MyIntProperty`) and
/// whose arguments open no scope of their own (a lambda or a comprehension), which could not be
/// inferred there. It only decides what gets inferred: whether the call declares a property is
/// still decided by the callee's resolved type, so a user's own `MyIntProperty` is rejected.
pub(crate) fn may_be_property_call(call: &ast::ExprCall) -> bool {
    let name = match call.func.as_ref() {
        Expr::Name(name) => name.id.as_str(),
        Expr::Attribute(attribute) => attribute.attr.as_str(),
        _ => return false,
    };
    PropertyKind::ALL
        .iter()
        .any(|(function, _)| name.ends_with(function))
        && !call.arguments.iter_source_order().any(|argument| {
            any_over_expr(argument.value(), |expr| {
                matches!(
                    expr,
                    Expr::Lambda(_)
                        | Expr::ListComp(_)
                        | Expr::SetComp(_)
                        | Expr::DictComp(_)
                        | Expr::Generator(_)
                        | Expr::Named(_)
                        | Expr::Yield(_)
                        | Expr::YieldFrom(_)
                        | Expr::Await(_)
                )
            })
        })
}

/// The value type of the annotation `call`, when it is a property declaration.
///
/// Called from type-expression inference once the call has been inferred, with `callee` the type
/// of its function and `argument_type` the inferred types of its arguments.
pub(crate) fn annotation_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    callee: Type<'db>,
    call: &ast::ExprCall,
    argument_type: impl Fn(&Expr) -> Type<'db>,
) -> Option<Type<'db>> {
    PropertyDeclaration::from_call(db, scope, callee, call, argument_type)
        .map(|declaration| declaration.value_type(db, env))
}

/// The property declaration that `definition` makes, when it is a class member annotated with a
/// call to one of the ten `bpy.props` functions (`count: IntProperty()`, `count: "IntProperty()"`).
///
/// The callee's type is read from the definition's own inference, so this never disagrees with
/// the annotation takeover in type-expression inference.
///
/// Inferring the annotation can look up a member of the class being declared
/// (`count: IntProperty(default=Group.count)` under `from __future__ import annotations`), which
/// asks this query again; such a cycle settles on "not a declaration".
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn property_declaration<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<PropertyDeclaration<'db>> {
    let DefinitionKind::AnnotatedAssignment(assignment) = definition.kind(db) else {
        return None;
    };
    let scope = definition.scope(db);
    if scope.scope(db).kind() != ScopeKind::Class {
        return None;
    }
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let annotation = assignment.annotation(&module);
    let parsed;
    let call = match annotation {
        Expr::Call(call) => call,
        Expr::StringLiteral(string) => {
            let literal = string.as_single_part_string()?;
            let source = source_text(db, definition.file(db));
            parsed = parsed_string_annotation(&source, literal).ok()?;
            let call = parsed.expr().as_call_expr()?;
            if !may_be_property_call(call)
                || !infer_definition_types(db, definition).is_string_annotation(annotation)
            {
                return None;
            }
            call
        }
        _ => return None,
    };
    let callee = definition_expression_type_in_scope(db, definition, &call.func, scope);
    PropertyDeclaration::from_call(db, scope, callee, call, |argument| {
        definition_expression_type_in_scope(db, definition, argument, scope)
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

/// Whether the class attribute `name` of `class` is hidden because it is an annotated property.
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
/// annotated property that declares it: `value: IntProperty()` on a Panel creates nothing.
pub(crate) fn hides_instance_member<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> bool {
    annotated_property(db, class, name).is_some() && is_ignored_host(db, class)
}

/// Check a write `target = ...` to the attribute `attribute` of an instance of `object_ty`, which
/// ty's own checks have accepted, against a read-only property: an annotated one, or a registered
/// one (`bpy.types.Scene.my_group = PointerProperty(...)`) when no class body declares the
/// attribute. Reports an `invalid-assignment` and returns `false` when the property is read-only.
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
    let Some(class) = object_ty
        .as_nominal_instance()
        .and_then(|instance| instance.class(db, env).class_literal(db).as_static())
    else {
        return true;
    };
    // The class nearest in the MRO whose body declares or assigns the attribute decides, so that a
    // subclass can replace a property with another one or with a plain class attribute. A body
    // that only reads the name (`alias = items`) does not.
    let owner = class.iter_mro(db, None).find_map(|base| match base {
        ClassBase::Class(base) => {
            let base = base.class_literal(db).as_static()?;
            let table = place_table(db, base.body_scope(db));
            let symbol = table.symbol(table.symbol_id(attribute)?);
            (symbol.is_declared() || symbol.is_bound()).then_some(base)
        }
        _ => None,
    });
    let read_only = match owner {
        Some(owner) => annotated_property(db, owner, attribute)
            .is_some_and(|declaration| declaration.is_read_only(db)),
        None => is_registered_read_only(db, context.program_file(), class, attribute),
    };
    if !read_only {
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

/// Whether overriding `name` of `superclass` with `definition` is never an incompatible override,
/// because one of the two is an annotated property.
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

/// The set of strings in a set literal such as `options={"ENUM_FLAG", "HIDDEN"}`.
fn string_set(expr: &Expr) -> Vec<String> {
    let Expr::Set(set) = expr else {
        return Vec::new();
    };
    set.elts
        .iter()
        .filter_map(Expr::as_string_literal_expr)
        .map(|string| string.value.to_string())
        .collect()
}

/// The identifiers of static enum items: a list or tuple of tuples whose first element is a string
/// literal, with `None` separators, written inline or through one constant of the same module.
///
/// An imported constant is not followed, so that a file's inference never depends on another
/// file's syntax tree; its items give `str`.
fn static_enum_items<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    items: &Expr,
) -> Option<Box<[Name]>> {
    if let Expr::Name(name) = items {
        let definitions = definitions_for_name(
            db,
            scope,
            name.id.as_str(),
            ImportAliasResolution::PreserveAliases,
        );
        let [resolved] = definitions.as_slice() else {
            return None;
        };
        let definition = resolved.definition()?;
        if definition.file(db) != scope.file(db) {
            return None;
        }
        let module = parsed_module(db, definition.python_file(db)).load(db);
        let value = match definition.kind(db) {
            DefinitionKind::Assignment(assignment) => assignment.value(&module),
            DefinitionKind::AnnotatedAssignment(assignment) => assignment.value(&module)?,
            _ => return None,
        };
        return literal_enum_items(value);
    }
    literal_enum_items(items)
}

fn literal_enum_items(items: &Expr) -> Option<Box<[Name]>> {
    let elements = match items {
        Expr::List(list) => &list.elts,
        Expr::Tuple(tuple) => &tuple.elts,
        _ => return None,
    };
    elements
        .iter()
        .filter(|element| !element.is_none_literal_expr())
        .map(|element| {
            let identifier = element.as_tuple_expr()?.elts.first()?;
            Some(Name::new(
                identifier.as_string_literal_expr()?.value.to_str(),
            ))
        })
        .collect()
}
