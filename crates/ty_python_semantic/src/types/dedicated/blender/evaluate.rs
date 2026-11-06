//! The Property evaluator: which Property declarations an expression evaluates to.
//!
//! Blender evaluates a class annotation and only needs the value to be a deferred property, so an
//! add-on can share declarations in many ways:
//!
//! ```python
//! COMMON = IntProperty(min=0)
//! ColorProperty = functools.partial(FloatVectorProperty, subtype="COLOR")
//! PROPS = {"weight": FloatProperty()}
//!
//! def translation(size):
//!     return FloatVectorProperty(subtype="TRANSLATION", size=size)
//!
//! class Settings(PropertyGroup):
//!     count: COMMON
//!     color: ColorProperty(size=4)
//!     weight: PROPS["weight"]
//!     offset: translation(2)
//! ```
//!
//! ty types every such value as the stubs' `_PropertyDeferred`, which says that it is a property
//! but not which one, and it doesn't infer what an unannotated wrapper returns. So the evaluator
//! follows names to their definitions (through the shared follower) and reads the declarations
//! there. Each constant and each wrapper function is read in its own tracked query, keyed on its
//! definition, so a file's inference reads another file's syntax tree only inside a query about
//! that file, and a cycle (a wrapper that calls itself) settles on "not a property".
//!
//! A wrapper or a `functools.partial` is a [`DeclarationTemplate`], filled with the arguments of
//! each call.

use ruff_db::parsed::{ParsedModuleRef, parsed_module, parsed_string_annotation};
use ruff_db::source::source_text;
use ruff_python_ast::name::Name;
use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::{NodeWithScopeRef, ScopeId, ScopeKind};
use ty_python_core::{ProgramFile, semantic_index};

use crate::types::infer::{
    infer_complete_scope_types, infer_definition_types, nearest_enclosing_class,
};
use crate::types::{
    KnownInstanceType, SpecialFormType, StaticClassLiteral, Type, UnionType,
    definition_expression_type_in_scope,
};
use crate::{Db, ProgramEnvironment};

use super::addon::is_first_party;
use super::addon_of_file;
use super::follow::{Resolved, resolve_expression};
use super::properties::{
    CallSite, DeclarationTemplate, Open, OptionReader, ParameterRef, PropertyDeclaration,
    may_be_property_call,
};
use super::symbols::{bpy_types_bases, is_property_deferred};

/// What a property value evaluates to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) enum PropertyValue<'db> {
    /// One of these declarations, such as the two returns of a wrapper.
    Declarations(Box<[PropertyDeclaration<'db>]>),
    /// A deferred property whose function or options the checker cannot read. It is still a
    /// property, of any type.
    Opaque,
}

impl<'db> PropertyValue<'db> {
    /// The type of the attribute Blender creates: the union of the declarations' value types, or
    /// `Any`.
    pub(super) fn value_type(&self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        match self {
            PropertyValue::Declarations(declarations) => UnionType::from_elements(
                db,
                env,
                declarations
                    .iter()
                    .map(|declaration| declaration.value_type(db, env)),
            ),
            PropertyValue::Opaque => Type::any(),
        }
    }

    /// The type a write to the attribute accepts.
    pub(super) fn write_type(&self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        match self {
            PropertyValue::Declarations(declarations) => UnionType::from_elements(
                db,
                env,
                declarations
                    .iter()
                    .map(|declaration| declaration.write_type(db, env)),
            ),
            PropertyValue::Opaque => Type::any(),
        }
    }

    /// Whether writing the attribute is an error, for any of the declarations.
    pub(super) fn is_read_only(&self, db: &'db dyn Db) -> bool {
        match self {
            PropertyValue::Declarations(declarations) => declarations
                .iter()
                .any(|declaration| declaration.is_read_only(db)),
            PropertyValue::Opaque => false,
        }
    }
}

/// Evaluates expressions in one scope, where `reader` reads the inferred types.
///
/// It never recurses into an expression by itself: an alias chain (`A = B`, `B = COMMON`) is
/// followed by the shared follower, which stops after a fixed number of assignments, and every
/// constant and wrapper it reaches is a query of its own, so a cycle between them settles on "not a
/// property".
pub(super) struct Evaluator<'a, 'db> {
    reader: OptionReader<'a, 'db>,
}

impl<'a, 'db> Evaluator<'a, 'db> {
    pub(super) fn new(
        db: &'db dyn Db,
        scope: ScopeId<'db>,
        argument_type: &'a dyn Fn(&Expr) -> Type<'db>,
    ) -> Self {
        Evaluator {
            reader: OptionReader {
                db,
                scope,
                argument_type,
            },
        }
    }

    fn db(&self) -> &'db dyn Db {
        self.reader.db
    }

    /// The declarations `expression` evaluates to, or `None` when it is not a property the
    /// checker can follow. The inferred type of `expression` itself is never asked for, as an
    /// annotation stores its type-expression type; only its parts are.
    pub(super) fn evaluate(&self, expression: &Expr) -> Option<PropertyValue<'db>> {
        match expression {
            Expr::Call(call) => self.evaluate_call(call, (self.reader.argument_type)(&call.func)),
            Expr::Name(_) | Expr::Attribute(_) => match self.constant(expression)? {
                ConstantValue::Property(value) => Some(value.clone()),
                ConstantValue::Container(_) | ConstantValue::Partial(_) => None,
            },
            Expr::Subscript(subscript) => self.evaluate_subscript(subscript),
            _ => None,
        }
    }

    /// The constant that the name or dotted name `reference` refers to.
    fn constant(&self, reference: &Expr) -> Option<&'db ConstantValue<'db>> {
        let Resolved::Constant(definition) =
            resolve_expression(self.db(), self.reader.scope, reference)?
        else {
            return None;
        };
        constant_value(self.db(), definition).as_ref()
    }

    /// `PROPS["weight"]` or `PAIR[0]`, where the constant is a dict, tuple or list literal and the
    /// key a literal.
    pub(super) fn evaluate_subscript(
        &self,
        subscript: &ast::ExprSubscript,
    ) -> Option<PropertyValue<'db>> {
        let ConstantValue::Container(entries) = self.constant(&subscript.value)? else {
            return None;
        };
        let key = match &*subscript.slice {
            Expr::StringLiteral(string) => Key::Str(Name::new(string.value.to_str())),
            index => {
                let (negative, number) = match index {
                    Expr::UnaryOp(ast::ExprUnaryOp {
                        op: ast::UnaryOp::USub,
                        operand,
                        ..
                    }) => (true, &**operand),
                    number => (false, number),
                };
                let Expr::NumberLiteral(ast::ExprNumberLiteral {
                    value: ast::Number::Int(int),
                    ..
                }) = number
                else {
                    return None;
                };
                let int = usize::try_from(int.as_u64()?).ok()?;
                Key::Index(if negative {
                    entries.len().checked_sub(int)?
                } else {
                    int
                })
            }
        };
        entries
            .iter()
            .find_map(|(entry, value)| (*entry == key).then_some(value))?
            .clone()
    }

    /// The declarations the call `callee(...)` evaluates to: a `bpy.props` function, a wrapper
    /// function of the project, or a `functools.partial` constant of a `bpy.props` function.
    pub(super) fn evaluate_call(
        &self,
        call: &ast::ExprCall,
        callee: Type<'db>,
    ) -> Option<PropertyValue<'db>> {
        let db = self.db();
        if let Some(declaration) = PropertyDeclaration::from_call(&self.reader, callee, call) {
            return Some(PropertyValue::Declarations(Box::new([declaration])));
        }
        if let Type::FunctionLiteral(function) = callee
            && is_first_party(db, function.program_file(db))
        {
            let wrapper = wrapper_template(db, function.definition(db)).as_ref()?;
            if wrapper.opaque {
                return Some(PropertyValue::Opaque);
            }
            let site = bind(
                &self.reader,
                call,
                &wrapper.positional,
                &wrapper.keyword_only,
                wrapper.kwargs,
            );
            return Some(PropertyValue::Declarations(
                wrapper
                    .returns
                    .iter()
                    .map(|template| template.instantiate(&site))
                    .collect(),
            ));
        }
        let ConstantValue::Partial(template) = self.constant(&call.func)? else {
            return None;
        };
        let site = bind(&self.reader, call, &[], &[], true);
        Some(PropertyValue::Declarations(Box::new([
            template.instantiate(&site)
        ])))
    }
}

/// The arguments of `call` bound to the parameters of what it calls: positional arguments to
/// `positional` in order, keywords by name, and the other keywords by their own names when `extra`
/// (a `**kwargs`, or the keywords a `functools.partial` passes on).
fn bind<'r, 'a, 'db>(
    reader: &'r OptionReader<'a, 'db>,
    call: &'r ast::ExprCall,
    positional: &[Name],
    keyword_only: &[Name],
    extra: bool,
) -> CallSite<'r, 'a, 'db> {
    let mut site = CallSite::none(reader);
    for (index, argument) in call.arguments.args.iter().enumerate() {
        if argument.is_starred_expr() {
            site.unpacks = true;
            break;
        }
        if let Some(parameter) = positional.get(index) {
            site.arguments.push((parameter.clone(), argument));
        }
    }
    for keyword in &call.arguments.keywords {
        match &keyword.arg {
            Some(name) => {
                let name = Name::new(name.as_str());
                if extra || positional.contains(&name) || keyword_only.contains(&name) {
                    site.arguments.push((name, &keyword.value));
                }
            }
            None => site.unpacks = true,
        }
    }
    site
}

/// A key of a container constant: a dict's string key, or a tuple's or list's index.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum Key {
    Str(Name),
    Index(usize),
}

/// What the value of a constant is, as far as properties go.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum ConstantValue<'db> {
    /// `COMMON = IntProperty()`.
    Property(PropertyValue<'db>),
    /// `PROPS = {"weight": FloatProperty(), "count": 3}`: each entry, a property or not.
    Container(Box<[(Key, Option<PropertyValue<'db>>)]>),
    /// `ColorProperty = functools.partial(FloatVectorProperty, subtype="COLOR")`.
    Partial(DeclarationTemplate<'db>),
}

/// The value of the constant `definition` (an assignment to a name), read in its own scope.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn constant_value<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Option<ConstantValue<'db>> {
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let value = match definition.kind(db) {
        DefinitionKind::Assignment(assignment) => assignment.value(&module),
        DefinitionKind::AnnotatedAssignment(assignment) => assignment.value(&module)?,
        _ => return None,
    };
    let env = ProgramEnvironment::from_file(definition.program_file(db));
    let scope = definition.scope(db);
    let argument_type =
        |expression: &Expr| definition_expression_type_in_scope(db, definition, expression, scope);
    let evaluator = Evaluator::new(db, scope, &argument_type);
    // An element that ty types as a deferred property is one, even if the checker can't tell which.
    let element = |element: &Expr| {
        evaluator.evaluate(element).or_else(|| {
            is_property_deferred(db, &env, argument_type(element)).then_some(PropertyValue::Opaque)
        })
    };
    match value {
        Expr::Dict(dict) => Some(ConstantValue::Container(
            dict.items
                .iter()
                .filter_map(|item| {
                    let key = item.key.as_ref()?.as_string_literal_expr()?;
                    Some((
                        Key::Str(Name::new(key.value.to_str())),
                        element(&item.value),
                    ))
                })
                .collect(),
        )),
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
            Some(ConstantValue::Container(
                elts.iter()
                    .enumerate()
                    .map(|(index, value)| (Key::Index(index), element(value)))
                    .collect(),
            ))
        }
        Expr::Call(call)
            if let Type::KnownInstance(KnownInstanceType::FunctoolsPartial(partial)) =
                argument_type(value) =>
        {
            // ty's partial type knows the wrapped function; the keywords it binds are read from
            // the `partial(...)` call itself.
            let wrapped = partial.wrapped(db).inner(db);
            let reader = OptionReader {
                db,
                scope,
                argument_type: &argument_type,
            };
            DeclarationTemplate::from_call(&reader, wrapped, call, &|_| None, Open::Partial)
                .map(ConstantValue::Partial)
        }
        value => element(value).map(ConstantValue::Property),
    }
}

/// A wrapper function: what each of its returns declares, with the parameters a call fills.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct WrapperTemplate<'db> {
    /// The parameters that a positional argument binds, in order.
    positional: Box<[Name]>,
    keyword_only: Box<[Name]>,
    /// Whether it takes `**kwargs`.
    kwargs: bool,
    /// One template per `return`.
    returns: Box<[DeclarationTemplate<'db>]>,
    /// Some `return` gives a deferred property the checker cannot read.
    opaque: bool,
}

/// The template of the function `definition`, when every `return` in its body (outside nested
/// functions and classes) evaluates to a property declaration.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn wrapper_template<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<WrapperTemplate<'db>> {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let file = definition.program_file(db);
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let function = function.node(&module);
    let mut returns = Returns::default();
    returns.visit_body(&function.body);
    if returns.values.is_empty() || returns.bare {
        return None;
    }

    let parameters = &function.parameters;
    let positional: Box<[Name]> = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .map(|parameter| Name::new(parameter.name().as_str()))
        .collect();
    let keyword_only: Box<[Name]> = parameters
        .kwonlyargs
        .iter()
        .map(|parameter| Name::new(parameter.name().as_str()))
        .collect();
    let kwargs = parameters.kwarg.as_ref().map(|kwarg| kwarg.name.as_str());

    let env = ProgramEnvironment::from_file(file);
    let body_scope = semantic_index(db, file)
        .node_scope(NodeWithScopeRef::Function(function))
        .to_scope_id(db, file);
    let inference = infer_complete_scope_types(db, body_scope);
    let argument_type = |expression: &Expr| {
        inference
            .try_expression_type(expression)
            .unwrap_or_else(Type::unknown)
    };
    let evaluator = Evaluator::new(db, body_scope, &argument_type);

    let mut templates = Vec::new();
    let mut opaque = false;
    for value in returns.values {
        if let Expr::Call(call) = value
            && let Some(template) = DeclarationTemplate::from_call(
                &evaluator.reader,
                argument_type(&call.func),
                call,
                &|value| parameter_ref(parameters, value),
                kwargs.map_or(Open::Closed, Open::Forwarded),
            )
        {
            templates.push(template);
            continue;
        }
        match evaluator.evaluate(value) {
            Some(PropertyValue::Declarations(declarations)) => {
                templates.extend(declarations.into_iter().map(DeclarationTemplate::fixed))
            }
            Some(PropertyValue::Opaque) => opaque = true,
            None if is_property_deferred(db, &env, argument_type(value)) => opaque = true,
            None => return None,
        }
    }
    Some(WrapperTemplate {
        positional,
        keyword_only,
        kwargs: kwargs.is_some(),
        returns: templates.into_boxed_slice(),
        opaque,
    })
}

/// The parameter of `parameters` that `value` names, with its default: `size` in
/// `def translation(size=3): return FloatVectorProperty(size=size)`.
fn parameter_ref<'e>(parameters: &'e ast::Parameters, value: &'e Expr) -> ParameterRef<'e> {
    let name = value.as_name_expr()?.id.as_str();
    parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .chain(&parameters.kwonlyargs)
        .find(|parameter| parameter.name().as_str() == name)
        .map(|parameter| (Name::new(name), parameter.default()))
}

/// The values of a function's `return` statements, outside nested functions and classes. A bare
/// `return` makes the function no wrapper.
#[derive(Default)]
struct Returns<'a> {
    values: Vec<&'a Expr>,
    bare: bool,
}

impl<'a> StatementVisitor<'a> for Returns<'a> {
    fn visit_stmt(&mut self, statement: &'a ast::Stmt) {
        match statement {
            ast::Stmt::Return(ast::StmtReturn { value, .. }) => match value {
                Some(value) => self.values.push(value),
                None => self.bare = true,
            },
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
            statement => walk_stmt(self, statement),
        }
    }
}

/// The value type of the annotation `call`, when it evaluates to a property.
///
/// Called from type-expression inference once the call has been inferred, with `callee` the type
/// of its function and `argument_type` the inferred types of its parts.
pub(crate) fn annotation_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    callee: Type<'db>,
    call: &ast::ExprCall,
    argument_type: impl Fn(&Expr) -> Type<'db>,
) -> Option<Type<'db>> {
    Evaluator::new(db, scope, &argument_type)
        .evaluate_call(call, callee)
        .map(|value| value.value_type(db, env))
}

/// Whether the annotation `x: callee(...)` of a member of `class` in `file`, which the evaluator
/// cannot read, is still a property of any type: its function is dynamic
/// (`getattr(bpy.props, name)()`), the class derives from a `bpy.types` class, and the file belongs
/// to an add-on, where Blender would evaluate it. A class with a dynamic base
/// (`class Local(bpy.types.PropertyGroup)` where `bpy` is a parameter) keeps ty's error.
///
/// The callee decides rather than the call's own type, so that the member query, which only sees
/// the types ty stored for the annotation's parts, agrees with the takeover.
pub(crate) fn is_opaque_call<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    callee: Type<'db>,
) -> bool {
    callee.is_dynamic()
        && bpy_types_bases(db, class).next().is_some()
        && addon_of_file(db, file).is_some()
}

/// The value type of the annotation `reference` (a name, dotted name or subscript, in `scope`),
/// when ty's value type of it, `ty`, says it is a deferred property: `x: COMMON`,
/// `x: props.COMMON`, `x: PROPS["weight"]`. `Any` when the evaluator cannot tell which.
pub(crate) fn reference_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    ty: Type<'db>,
    reference: &Expr,
) -> Option<Type<'db>> {
    if !is_property_value_type(db, env, ty) {
        return None;
    }
    let argument_type = |_: &Expr| Type::unknown();
    let value = Evaluator::new(db, scope, &argument_type)
        .evaluate(reference)
        .unwrap_or(PropertyValue::Opaque);
    Some(value.value_type(db, env))
}

/// The value type of the annotation `subscript` in `scope`, when `container` (ty's value type of
/// what it subscripts) holds deferred properties: `x: PROPS["weight"]`. `Any` when the evaluator
/// cannot tell which.
pub(crate) fn subscript_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    subscript: &ast::ExprSubscript,
) -> Type<'db> {
    let argument_type = |_: &Expr| Type::unknown();
    Evaluator::new(db, scope, &argument_type)
        .evaluate_subscript(subscript)
        .unwrap_or(PropertyValue::Opaque)
        .value_type(db, env)
}

/// Whether `ty` is a deferred property, or a union of deferred properties and dynamic types.
fn is_property_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    match ty {
        Type::Union(union) => {
            let elements = union.elements(db);
            elements
                .iter()
                .all(|element| element.is_dynamic() || is_property_deferred(db, env, *element))
                && elements
                    .iter()
                    .any(|element| is_property_deferred(db, env, *element))
        }
        ty => is_property_deferred(db, env, ty),
    }
}

/// Whether subscripting `container` gives deferred properties: a tuple of them, or a dict or list
/// whose values are (`dict[str, _PropertyDeferred]`).
pub(crate) fn is_property_container<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    container: Type<'db>,
) -> bool {
    let Type::NominalInstance(instance) = container else {
        return false;
    };
    if let Some(tuple) = container.exact_tuple_instance_spec(db)
        && let Some(tuple) = tuple.as_fixed_length()
    {
        let elements = tuple.all_elements();
        return !elements.is_empty()
            && is_property_value_type(
                db,
                env,
                UnionType::from_elements(db, env, elements.iter().copied()),
            );
    }
    instance
        .class(db, env)
        .into_generic_alias()
        .and_then(|alias| alias.specialization(db).types(db).last().copied())
        .is_some_and(|values| is_property_value_type(db, env, values))
}

/// The property that the class member `definition` declares, when its annotation evaluates to one
/// (`count: IntProperty()`, `count: "IntProperty()"`, `count: COMMON`, `count: wrapper(2)`, and in
/// the stubs `filepath: Annotated[str, StringProperty()]`).
///
/// It runs the same evaluator as the annotation takeover in type-expression inference, and reads
/// the callee's type from the definition's own inference, so the two never disagree. An opaque
/// value still counts as a property.
///
/// Inferring the annotation can look up a member of the class being declared
/// (`count: IntProperty(default=Group.count)` under `from __future__ import annotations`), which
/// asks this query again; such a cycle settles on "not a declaration".
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn property_value<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<PropertyValue<'db>> {
    let scope = definition.scope(db);
    let file = definition.program_file(db);
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let argument_type =
        |expression: &Expr| definition_expression_type_in_scope(db, definition, expression, scope);
    let evaluator = Evaluator::new(db, scope, &argument_type);
    with_member_annotation(db, definition, &module, |annotation, quoted| {
        match annotation {
            Expr::Call(call) => {
                let callee = argument_type(&call.func);
                evaluator.evaluate_call(call, callee).or_else(|| {
                    // As in the takeover, only an unquoted annotation can be opaque.
                    let class = nearest_enclosing_class(db, semantic_index(db, file), scope)?;
                    (quoted.is_none() && is_opaque_call(db, file, class, callee))
                        .then_some(PropertyValue::Opaque)
                })
            }
            // Stubs declare the properties of Blender's mixins (`ImportHelper.filepath`) as
            // `Annotated[str, StringProperty(...)]`, which reads as the value type for any type
            // checker. In an add-on, Blender strips `Annotated`, so there it stays an ordinary
            // annotation.
            Expr::Subscript(subscript) if quoted.is_none() && !is_first_party(db, file) => {
                annotated_metadata(subscript, &argument_type)?
                    .iter()
                    .find_map(|metadata| match metadata {
                        Expr::Call(call) => {
                            evaluator.evaluate_call(call, argument_type(&call.func))
                        }
                        _ => None,
                    })
            }
            // Only a project's own classes can name a constant property; this keeps the classes
            // of typeshed and the stubs from following every annotation's name.
            Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_) if is_first_party(db, file) => {
                evaluator.evaluate(annotation)
            }
            _ => None,
        }
    })
}

/// The metadata of `Annotated[T, *metadata]`, where `argument_type` gives the inferred types of its
/// parts, or `None` when `subscript` is not a subscript of `typing.Annotated` with a call in its
/// metadata. The syntax is checked first, so that most annotations need no inferred type.
fn annotated_metadata<'e, 'db>(
    subscript: &'e ast::ExprSubscript,
    argument_type: &impl Fn(&Expr) -> Type<'db>,
) -> Option<&'e [Expr]> {
    let Expr::Tuple(tuple) = &*subscript.slice else {
        return None;
    };
    let metadata = tuple.elts.get(1..)?;
    (metadata.iter().any(Expr::is_call_expr)
        && argument_type(&subscript.value) == Type::SpecialForm(SpecialFormType::Annotated))
    .then_some(metadata)
}

/// Read the annotation of the class member `definition` (`count: IntProperty()` in a class body)
/// with `read`, which also gets the range of the string literal when the annotation is quoted
/// (`count: "IntProperty()"`). The expressions of a quoted annotation have ranges within the
/// string, not within the file. A quoted call is only read when it may declare a property and ty
/// inferred it (see [`may_be_property_call`]), so that its parts have types.
pub(super) fn with_member_annotation<R>(
    db: &dyn Db,
    definition: Definition<'_>,
    module: &ParsedModuleRef,
    read: impl FnOnce(&Expr, Option<TextRange>) -> Option<R>,
) -> Option<R> {
    let DefinitionKind::AnnotatedAssignment(assignment) = definition.kind(db) else {
        return None;
    };
    if definition.scope(db).scope(db).kind() != ScopeKind::Class {
        return None;
    }
    let annotation = assignment.annotation(module);
    let Expr::StringLiteral(string) = annotation else {
        return read(annotation, None);
    };
    let literal = string.as_single_part_string()?;
    let source = source_text(db, definition.file(db));
    let parsed = parsed_string_annotation(&source, literal).ok()?;
    let expression = parsed.expr();
    if let Expr::Call(call) = expression
        && !(may_be_property_call(call)
            && infer_definition_types(db, definition).is_string_annotation(annotation))
    {
        return None;
    }
    read(expression, Some(string.range()))
}
