//! Property declarations: `count: IntProperty(min=0)` on a Blender class.
//!
//! Blender turns an annotation that is a call to a `bpy.props` function into a property, and the
//! instance attribute then holds the property's value (an `int` here), not the `_PropertyDeferred`
//! the stubs return. The annotation takeover in type-expression inference asks this module for that
//! value type.
//!
//! [`PropertyDeclaration`] describes one declaration, whether it annotates a class member
//! ([`property_declaration`]) or is assigned to a Blender class at runtime (found by the Add-on
//! index). Which declaration an attribute of an instance stands for is decided by the Property
//! member module.

use ruff_db::parsed::{parsed_module, parsed_string_annotation};
use ruff_db::source::source_text;
use ruff_python_ast::helpers::any_over_expr;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Expr};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::{ScopeId, ScopeKind};

use crate::types::definition_resolution::{ImportAliasResolution, definitions_for_name};
use crate::types::infer::infer_definition_types;
use crate::types::{
    KnownClass, KnownUnion, StaticClassLiteral, Type, UnionType,
    definition_expression_type_in_scope,
};
use crate::{Db, ProgramEnvironment};

use super::symbols::{blender_function_name, bpy_types_bases};
use super::vector::{Element, Known, VectorShape, module_class, read_size, read_string};

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

/// Whether `callee` is one of the ten `bpy.props` functions that declare a property.
pub(super) fn is_property_function<'db>(db: &'db dyn Db, callee: Type<'db>) -> bool {
    PropertyKind::of_callee(db, callee).is_some()
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
    /// `get=` without `set=`: Blender makes the property read-only.
    get_without_set: bool,
    /// `StringProperty(subtype="BYTE_STRING")`: the value is `bytes`.
    byte_string: bool,
    /// The `size=` and `subtype=` of a vector property, or `None` for any other kind.
    vector: Option<VectorShape>,
    /// The inferred type of a pointer's or collection's `type=`, or `None` when it is missing.
    target: Option<Type<'db>>,
}

impl<'db> PropertyDeclaration<'db> {
    /// Describe the property that `call` declares, or `None` when `callee` (the inferred type of
    /// the call's function) is not one of the ten `bpy.props` functions.
    ///
    /// `items=` and `options=` are read from the syntax, because ty widens list and set literals to
    /// `list[tuple[str, str, str]]` and `set[str]`. `size=` and `subtype=` are read from the syntax,
    /// else from literal types. Only keyword arguments are read, as Blender only accepts them for
    /// these parameters. `scope` is where a module constant passed as `items=` is looked up.
    /// `type=` is read from its inferred type, which `argument_type` gives, so that aliases and
    /// forward references to the target class work.
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
        let unpacks_mapping = call.arguments.keywords.iter().any(|kw| kw.arg.is_none());
        let subtype = Known::read(keyword("subtype"), unpacks_mapping, |subtype| {
            read_string(db, subtype, &argument_type)
        });
        let element = match kind {
            PropertyKind::BoolVector => Some(Element::Bool),
            PropertyKind::FloatVector => Some(Element::Float),
            PropertyKind::IntVector => Some(Element::Int),
            _ => None,
        };

        Some(PropertyDeclaration {
            kind,
            enum_items: (kind == PropertyKind::Enum)
                .then(|| keyword("items").and_then(|items| static_enum_items(db, scope, items)))
                .flatten(),
            enum_flag: options.iter().any(|option| option == "ENUM_FLAG"),
            read_only_option: options.iter().any(|option| option == "READ_ONLY"),
            get_without_set: keyword("get").is_some() && keyword("set").is_none(),
            byte_string: kind == PropertyKind::String
                && subtype == Known::Value(Name::new_static("BYTE_STRING")),
            vector: element.map(|element| VectorShape {
                element,
                size: Known::read(keyword("size"), unpacks_mapping, |size| {
                    read_size(db, size, &argument_type)
                }),
                subtype,
            }),
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
                module_class(db, env, "bpy.types", "bpy_prop_collection_idprop").map_or_else(
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
            PropertyKind::BoolVector | PropertyKind::FloatVector | PropertyKind::IntVector => self
                .vector
                .as_ref()
                .map_or_else(Type::unknown, |vector| vector.value_type(db, env)),
        }
    }

    /// The type that a write to the attribute accepts. It is the value type, except for a vector,
    /// which also accepts a nested sequence (`s.location = (0.0, 0.0, 1.0)` where `s.location` is a
    /// `Vector`).
    pub(super) fn write_type(&self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        match &self.vector {
            Some(vector) => vector.write_type(db, env),
            None => self.value_type(db, env),
        }
    }

    /// Whether writing the attribute is an error: a collection, a pointer to a `PropertyGroup`
    /// (Blender creates the group with its owner), or any property with `options={"READ_ONLY"}` or
    /// with a `get=` and no `set=`.
    pub(super) fn is_read_only(&self, db: &'db dyn Db) -> bool {
        self.read_only_option
            || self.get_without_set
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
