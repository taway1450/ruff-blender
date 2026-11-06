//! Property declarations: `count: IntProperty(min=0)` on a Blender class.
//!
//! Blender turns an annotation that evaluates to a call of a `bpy.props` function into a property,
//! and the instance attribute then holds the property's value (an `int` here), not the
//! `_PropertyDeferred` the stubs return.
//!
//! [`PropertyDeclaration`] describes one declaration: its kind and the options that decide its
//! type. A [`DeclarationTemplate`] is a declaration whose options may still come from a call, as in
//! a wrapper function's `return FloatVectorProperty(size=size)` or a
//! `functools.partial(FloatVectorProperty, subtype="COLOR")`; it is filled at each call. Which
//! expression evaluates to which declarations is decided by the Property evaluator.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::helpers::any_over_expr;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Expr};
use ty_python_core::definition::DefinitionKind;
use ty_python_core::scope::ScopeId;

use crate::types::definition_resolution::{ImportAliasResolution, definitions_for_name};
use crate::types::{KnownClass, KnownUnion, StaticClassLiteral, Type, UnionType};
use crate::{Db, ProgramEnvironment};

use super::symbols::{blender_function_name, bpy_types_bases};
use super::vector::{Element, Known, VectorShape, module_class, read_size, read_string};

/// The ten functions of `bpy.props` that declare a property.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) enum PropertyKind {
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
    pub(super) fn of_callee<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<Self> {
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
///
/// Each option that decides the value type is [`Known`]: not written (Blender's default applies),
/// written with a value the checker reads, or written in a way it cannot read (a parameter it
/// cannot follow, or a `**mapping` that may hold it).
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct PropertyDeclaration<'db> {
    kind: PropertyKind,
    /// The identifiers of an enum's static `items=`.
    items: Known<Box<[Name]>>,
    /// `options={"ENUM_FLAG", "READ_ONLY"}`.
    options: Known<Box<[Name]>>,
    /// Whether `get=` and `set=` are written.
    get: Known<()>,
    set: Known<()>,
    /// `subtype=`: a vector's shape, or `BYTE_STRING` for a `StringProperty`.
    subtype: Known<Name>,
    /// A vector's `size=`, its dimensions.
    size: Known<Box<[u8]>>,
    /// The inferred type of a pointer's or collection's `type=`.
    target: Known<Type<'db>>,
}

impl<'db> PropertyDeclaration<'db> {
    /// Describe the property that `call` declares, or `None` when `callee` (the inferred type of
    /// the call's function) is not one of the ten `bpy.props` functions.
    pub(super) fn from_call(
        reader: &OptionReader<'_, 'db>,
        callee: Type<'db>,
        call: &ast::ExprCall,
    ) -> Option<Self> {
        let template =
            DeclarationTemplate::from_call(reader, callee, call, &|_| None, Open::Closed)?;
        Some(template.instantiate(&CallSite::none(reader)))
    }

    /// The type of the attribute Blender creates from this declaration.
    pub(super) fn value_type(&self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        match self.kind {
            PropertyKind::Bool => KnownClass::Bool.to_instance(db, env),
            PropertyKind::Int => KnownClass::Int.to_instance(db, env),
            // What the annotation `float` means, so that `assert_type(x, float)` holds.
            PropertyKind::Float => KnownUnion::Float.to_type(db, env),
            PropertyKind::String
                if self.subtype == Known::Value(Name::new_static("BYTE_STRING")) =>
            {
                KnownClass::Bytes.to_instance(db, env)
            }
            PropertyKind::String => KnownClass::Str.to_instance(db, env),
            PropertyKind::Enum => {
                // `ENUM_FLAG` makes the value a set; without knowing the options, it is either.
                let Some(options) = self.options.clone().or_default(Box::default()) else {
                    return Type::any();
                };
                let identifier = match &self.items {
                    Known::Value(items) if !items.is_empty() => UnionType::from_elements(
                        db,
                        env,
                        items
                            .iter()
                            .map(|item| Type::string_literal(db, item.as_str())),
                    ),
                    _ => KnownClass::Str.to_instance(db, env),
                };
                if options.iter().any(|option| option == "ENUM_FLAG") {
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
                .vector()
                .map_or_else(Type::unknown, |vector| vector.value_type(db, env)),
        }
    }

    /// The type that a write to the attribute accepts. It is the value type, except for a vector,
    /// which also accepts a nested sequence (`s.location = (0.0, 0.0, 1.0)` where `s.location` is a
    /// `Vector`).
    pub(super) fn write_type(&self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        match self.vector() {
            Some(vector) => vector.write_type(db, env),
            None => self.value_type(db, env),
        }
    }

    /// Whether writing the attribute is an error: a collection, a pointer to a `PropertyGroup`
    /// (Blender creates the group with its owner), or any property with `options={"READ_ONLY"}` or
    /// with a `get=` and no `set=`. Options the checker cannot read never make it read-only.
    pub(super) fn is_read_only(&self, db: &'db dyn Db) -> bool {
        let read_only_option = matches!(&self.options, Known::Value(options)
            if options.iter().any(|option| option == "READ_ONLY"));
        let get_without_set = self.get == Known::Value(()) && self.set == Known::Absent;
        read_only_option
            || get_without_set
            || match self.kind {
                PropertyKind::Collection => true,
                PropertyKind::Pointer => matches!(
                    self.target_class(db),
                    Some((_, PointerTarget::PropertyGroup))
                ),
                _ => false,
            }
    }

    /// The shape of a vector property, or `None` for any other kind.
    fn vector(&self) -> Option<VectorShape> {
        let element = match self.kind {
            PropertyKind::BoolVector => Element::Bool,
            PropertyKind::FloatVector => Element::Float,
            PropertyKind::IntVector => Element::Int,
            _ => return None,
        };
        Some(VectorShape {
            element,
            size: self.size.clone(),
            subtype: self.subtype.clone(),
        })
    }

    /// The class `type=` names, when it is an ID or a `PropertyGroup` class.
    fn target_class(&self, db: &'db dyn Db) -> Option<(StaticClassLiteral<'db>, PointerTarget)> {
        let Known::Value(Type::ClassLiteral(class)) = self.target else {
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

/// Reads the options of a call: `items=` and `options=` from the syntax, because ty widens list and
/// set literals to `list[tuple[str, str, str]]` and `set[str]`; `size=` and `subtype=` from the
/// syntax, else from literal types; `type=` from its inferred type, so that aliases and forward
/// references to the target class work. `scope` is where a module constant passed as `items=` is
/// looked up, and `argument_type` gives the inferred type of an expression of the call.
pub(super) struct OptionReader<'a, 'db> {
    pub(super) db: &'db dyn Db,
    pub(super) scope: ScopeId<'db>,
    pub(super) argument_type: &'a dyn Fn(&Expr) -> Type<'db>,
}

impl<'db> OptionReader<'_, 'db> {
    fn items(&self, items: &Expr) -> Option<Box<[Name]>> {
        static_enum_items(self.db, self.scope, items)
    }

    fn options(&self, options: &Expr) -> Option<Box<[Name]>> {
        string_set(options)
    }

    fn present(&self, _callback: &Expr) -> Option<()> {
        Some(())
    }

    fn subtype(&self, subtype: &Expr) -> Option<Name> {
        read_string(self.db, subtype, self.argument_type)
    }

    fn size(&self, size: &Expr) -> Option<Box<[u8]>> {
        read_size(self.db, size, self.argument_type)
    }

    fn target(&self, target: &Expr) -> Option<Type<'db>> {
        Some((self.argument_type)(target))
    }
}

/// Which options of a template's call a later call can give.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum Open<'a> {
    /// Only parameters written as an option's value (`size=size` in a wrapper's body).
    Closed,
    /// Also every option the call doesn't write, when it unpacks the wrapper's own `**kwargs`
    /// (named here): `def vector(**kwargs): return FloatVectorProperty(**kwargs)`.
    Forwarded(&'a str),
    /// Every option, as a keyword of the later call replaces it:
    /// `functools.partial(FloatVectorProperty, subtype="COLOR")`.
    Partial,
}

/// A wrapper's parameter written as an option's value, and its default.
pub(super) type ParameterRef<'e> = Option<(Name, Option<&'e Expr>)>;

/// An option of a [`DeclarationTemplate`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) enum Slot<T> {
    /// Read where the template is written.
    Fixed(Known<T>),
    /// What a call binds to `parameter` (a wrapper's parameter, or a keyword the call passes on),
    /// else `default`.
    Parameter { parameter: Name, default: Known<T> },
}

impl<T: Clone> Slot<T> {
    fn fill(&self, site: &CallSite<'_, '_, '_>, read: impl Fn(&Expr) -> Option<T>) -> Known<T> {
        match self {
            Slot::Fixed(known) => known.clone(),
            Slot::Parameter { parameter, default } => match site.bound(parameter) {
                Some(argument) => Known::from_read(read(argument)),
                None if site.unpacks => Known::Unknown,
                None => default.clone(),
            },
        }
    }
}

/// A property declaration whose options may come from a later call.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct DeclarationTemplate<'db> {
    kind: PropertyKind,
    items: Slot<Box<[Name]>>,
    options: Slot<Box<[Name]>>,
    get: Slot<()>,
    set: Slot<()>,
    subtype: Slot<Name>,
    size: Slot<Box<[u8]>>,
    target: Slot<Type<'db>>,
}

impl<'db> DeclarationTemplate<'db> {
    /// The template that `call` writes, or `None` when `callee` is not one of the ten `bpy.props`
    /// functions. `parameter` says whether an option's value is a parameter of the enclosing
    /// wrapper, and `open` which unwritten options a later call gives.
    ///
    /// Only keyword arguments are read, as Blender only accepts them for these parameters. A
    /// `**mapping` the checker can't read makes every option that isn't written out unknown.
    pub(super) fn from_call<'e>(
        reader: &OptionReader<'_, 'db>,
        callee: Type<'db>,
        call: &'e ast::ExprCall,
        parameter: &dyn Fn(&'e Expr) -> ParameterRef<'e>,
        open: Open<'_>,
    ) -> Option<Self> {
        let kind = PropertyKind::of_callee(reader.db, callee)?;
        let mut forwards = matches!(open, Open::Partial);
        let mut unpacks_mapping = false;
        for keyword in &call.arguments.keywords {
            if keyword.arg.is_some() {
                continue;
            }
            match (&keyword.value, open) {
                (Expr::Name(name), Open::Forwarded(kwargs)) if name.id.as_str() == kwargs => {
                    forwards = true;
                }
                _ => unpacks_mapping = true,
            }
        }
        let slots = SlotReader {
            call,
            parameter,
            open,
            forwards,
            unpacks_mapping,
        };
        Some(DeclarationTemplate {
            kind,
            items: slots.slot("items", |e| reader.items(e)),
            options: slots.slot("options", |e| reader.options(e)),
            get: slots.slot("get", |e| reader.present(e)),
            set: slots.slot("set", |e| reader.present(e)),
            subtype: slots.slot("subtype", |e| reader.subtype(e)),
            size: slots.slot("size", |e| reader.size(e)),
            target: slots.slot("type", |e| reader.target(e)),
        })
    }

    /// The template of a declaration with nothing left to fill.
    pub(super) fn fixed(declaration: PropertyDeclaration<'db>) -> Self {
        let PropertyDeclaration {
            kind,
            items,
            options,
            get,
            set,
            subtype,
            size,
            target,
        } = declaration;
        DeclarationTemplate {
            kind,
            items: Slot::Fixed(items),
            options: Slot::Fixed(options),
            get: Slot::Fixed(get),
            set: Slot::Fixed(set),
            subtype: Slot::Fixed(subtype),
            size: Slot::Fixed(size),
            target: Slot::Fixed(target),
        }
    }

    /// The declaration that a call made at `site` gives.
    pub(super) fn instantiate(&self, site: &CallSite<'_, '_, 'db>) -> PropertyDeclaration<'db> {
        let reader = site.reader;
        PropertyDeclaration {
            kind: self.kind,
            items: self.items.fill(site, |e| reader.items(e)),
            options: self.options.fill(site, |e| reader.options(e)),
            get: self.get.fill(site, |e| reader.present(e)),
            set: self.set.fill(site, |e| reader.present(e)),
            subtype: self.subtype.fill(site, |e| reader.subtype(e)),
            size: self.size.fill(site, |e| reader.size(e)),
            target: self.target.fill(site, |e| reader.target(e)),
        }
    }
}

/// Reads the slots of one call that writes a template.
struct SlotReader<'e, 'p> {
    call: &'e ast::ExprCall,
    parameter: &'p dyn Fn(&'e Expr) -> ParameterRef<'e>,
    open: Open<'p>,
    /// Options that the call doesn't write come from a later call.
    forwards: bool,
    unpacks_mapping: bool,
}

impl SlotReader<'_, '_> {
    /// The slot of the keyword `option`, whose value `read` reads.
    fn slot<T>(&self, option: &str, read: impl Fn(&Expr) -> Option<T>) -> Slot<T> {
        let written = self
            .call
            .arguments
            .find_keyword(option)
            .map(|keyword| &keyword.value);
        let known = match written {
            Some(value) => {
                if let Some((parameter, default)) = (self.parameter)(value) {
                    let default =
                        default.map_or(Known::Unknown, |default| Known::from_read(read(default)));
                    return Slot::Parameter { parameter, default };
                }
                Known::from_read(read(value))
            }
            None if self.unpacks_mapping => Known::Unknown,
            None => Known::Absent,
        };
        if matches!(self.open, Open::Partial) || (written.is_none() && self.forwards) {
            Slot::Parameter {
                parameter: Name::new(option),
                default: known,
            }
        } else {
            Slot::Fixed(known)
        }
    }
}

/// A call of a template: the arguments it binds to each parameter, read with `reader`.
pub(super) struct CallSite<'r, 'a, 'db> {
    pub(super) reader: &'r OptionReader<'a, 'db>,
    pub(super) arguments: Vec<(Name, &'r Expr)>,
    /// The call unpacks a `*sequence` or `**mapping`, so any parameter may be bound.
    pub(super) unpacks: bool,
}

impl<'r, 'a, 'db> CallSite<'r, 'a, 'db> {
    /// A site that binds nothing, for a template with nothing to fill.
    pub(super) fn none(reader: &'r OptionReader<'a, 'db>) -> Self {
        CallSite {
            reader,
            arguments: Vec::new(),
            unpacks: false,
        }
    }

    fn bound(&self, parameter: &Name) -> Option<&'r Expr> {
        self.arguments
            .iter()
            .find_map(|(name, argument)| (name == parameter).then_some(*argument))
    }
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

/// The strings in a set literal such as `options={"ENUM_FLAG", "HIDDEN"}`, or `None` for any other
/// expression.
fn string_set(expr: &Expr) -> Option<Box<[Name]>> {
    let Expr::Set(set) = expr else {
        return None;
    };
    Some(
        set.elts
            .iter()
            .filter_map(Expr::as_string_literal_expr)
            .map(|string| Name::new(string.value.to_str()))
            .collect(),
    )
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
