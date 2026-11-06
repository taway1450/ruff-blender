//! The property rules: property declarations that Blender rejects when it registers them, or that it
//! silently ignores.
//!
//! - `blender-invalid-property`: a `bpy.props` call that Blender rejects at registration
//!   (`EnumProperty(items=AB, default="C")`, `IntProperty(set=...)` without `get=`, ...). The checks
//!   that need no owner run at every call where it is written (module constants, lambdas,
//!   comprehensions and annotations outside a class body included), so a broken shared declaration
//!   is reported once, where it is defined. The old `x = IntProperty()` form in a class body is the
//!   exception: it only gets `blender-legacy-property`. The data-block check needs the
//!   owner and runs only on direct annotations and registrations.
//! - `blender-unsupported-property-host`: an annotated property in the body of an Ignored host (a
//!   Panel), which creates no property.
//! - `blender-legacy-property`: `x = IntProperty()` in the body of a class with a `bpy.types` base,
//!   the form Blender stopped reading in 2.80.
//! - `blender-unsupported-property-target`: `bpy.types.Area.x = IntProperty()`, a registration on a
//!   class that keeps the declaration as a plain class attribute.
//!
//! Each line gets one rule: a declaration that Blender never evaluates (on an Ignored host, in the
//! legacy form, or on an unsupported target) is not checked for `blender-invalid-property`.
//!
//! What the stubs already reject (`IntProperty(default=1.5)`, a `type=` of the wrong kind, a callback
//! with the wrong number of parameters, an unknown `subtype=`) is left to them, and `min` / `max` are
//! never checked, because Blender accepts any of them.

use ruff_db::parsed::ParsedModuleRef;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::scope::{NodeWithScopeRef, ScopeId};
use ty_python_core::{SemanticIndex, global_scope, semantic_index};

use crate::Db;
use crate::lint::LintMetadata;
use crate::types::context::InferContext;
use crate::types::infer::{infer_complete_scope_types, nearest_enclosing_class};
use crate::types::{StaticClassLiteral, Type, definition_expression_type_in_scope};

use super::evaluate::{property_value, with_member_annotation};
use super::index::{RegisteredProperty, file_facts};
use super::lints::{
    BLENDER_INVALID_PROPERTY, BLENDER_LEGACY_PROPERTY, BLENDER_UNSUPPORTED_PROPERTY_HOST,
    BLENDER_UNSUPPORTED_PROPERTY_TARGET,
};
use super::member::is_ignored_host;
use super::properties::{
    EnumItems, Known, OptionReader, PointerTarget, PropertyDeclaration, PropertyKind, StaticItems,
};
use super::symbols::bpy_types_bases;
use super::vector::{int_literal, int_type, read_dimensions, valid_size};

/// Report the property rules of the file `context` checks, whose syntax tree is `module`.
pub(super) fn check_properties(context: &InferContext<'_, '_>, module: &ParsedModuleRef) {
    let db = context.db();
    let file = context.program_file();
    let mut checker = Checker {
        context,
        module,
        index: semantic_index(db, file),
        registrations: &file_facts(db, file).registered_properties,
        scope: global_scope(db, file),
        class: None,
    };
    checker.visit_body(module.suite());
}

/// A problem Blender has with a property declaration, and the range it is reported on.
struct Problem {
    range: TextRange,
    message: String,
}

impl Problem {
    fn new(range: impl Ranged, message: impl Into<String>) -> Self {
        Problem {
            range: range.range(),
            message: message.into(),
        }
    }
}

/// Walks a file's statements, keeping track of the scope (and class body) it is in.
struct Checker<'c, 'db, 'ast> {
    context: &'c InferContext<'db, 'ast>,
    module: &'c ParsedModuleRef,
    index: &'db SemanticIndex<'db>,
    /// The registrations in this file that the Add-on index found.
    registrations: &'db [RegisteredProperty<'db>],
    scope: ScopeId<'db>,
    /// The class whose body the statements belong to.
    class: Option<StaticClassLiteral<'db>>,
}

impl<'db> Checker<'_, 'db, '_> {
    fn db(&self) -> &'db dyn Db {
        self.context.db()
    }

    fn report(&self, lint: &'static LintMetadata, problem: Problem) {
        if let Some(builder) = self.context.report_lint(lint, problem.range) {
            builder.into_diagnostic(problem.message);
        }
    }

    /// Report problems of `blender-invalid-property`.
    ///
    /// When the registration cluster adds `blender-invalid-operator-property`, a declaration owned by
    /// an Operator or a Macro is reported with that rule instead, so that each line gets one rule.
    fn report_invalid(&self, problems: Vec<Problem>) {
        for problem in problems {
            self.report(&BLENDER_INVALID_PROPERTY, problem);
        }
    }

    /// Visit `body` as the body of `scope`, the body of `class` when it is a class body.
    fn visit_scope(
        &mut self,
        scope: ScopeId<'db>,
        class: Option<StaticClassLiteral<'db>>,
        body: &[ast::Stmt],
    ) {
        self.in_scope(scope, class, |checker| checker.visit_body(body));
    }

    /// Run `visit` with `scope` (and `class`) as the current scope.
    fn in_scope(
        &mut self,
        scope: ScopeId<'db>,
        class: Option<StaticClassLiteral<'db>>,
        visit: impl FnOnce(&mut Self),
    ) {
        let outer = (self.scope, self.class);
        (self.scope, self.class) = (scope, class);
        visit(self);
        (self.scope, self.class) = outer;
    }

    /// The scope that `node` (a class, function, lambda or comprehension of this file) opens.
    fn node_scope(&self, node: NodeWithScopeRef<'_>) -> ScopeId<'db> {
        self.index
            .node_scope(node)
            .to_scope_id(self.db(), self.context.program_file())
    }

    /// A comprehension (`node`), whose first iterable belongs to the enclosing scope and everything
    /// else, `elements` included, to its own: `[EnumProperty(...) for _ in range(2)]`.
    fn visit_comprehension(
        &mut self,
        node: NodeWithScopeRef<'_>,
        generators: &[ast::Comprehension],
        elements: &[&Expr],
    ) {
        let Some((first, rest)) = generators.split_first() else {
            return;
        };
        self.visit_expr(&first.iter);
        let scope = self.node_scope(node);
        self.in_scope(scope, None, |checker| {
            for condition in &first.ifs {
                checker.visit_expr(condition);
            }
            for generator in rest {
                checker.visit_expr(&generator.iter);
                for condition in &generator.ifs {
                    checker.visit_expr(condition);
                }
            }
            for element in elements {
                checker.visit_expr(element);
            }
        });
    }

    /// Whether `call`, in the current scope, calls one of the `bpy.props` functions.
    fn is_property_call(&self, call: &ast::ExprCall) -> bool {
        let inference = infer_complete_scope_types(self.db(), self.scope);
        inference
            .try_expression_type(&*call.func)
            .and_then(|callee| PropertyKind::of_callee(self.db(), callee))
            .is_some()
    }

    /// Check a `bpy.props` call in the current scope. `data_block_owner` names the owner when it
    /// cannot hold data-block pointers.
    fn check_call(&self, call: &ast::ExprCall, data_block_owner: Option<&str>) {
        let inference = infer_complete_scope_types(self.db(), self.scope);
        let argument_type = |expression: &Expr| {
            inference
                .try_expression_type(expression)
                .unwrap_or_else(Type::unknown)
        };
        self.check_call_with(call, data_block_owner, &argument_type);
    }

    /// Check a `bpy.props` call in the current scope, whose parts have the types `argument_type`
    /// gives.
    fn check_call_with(
        &self,
        call: &ast::ExprCall,
        data_block_owner: Option<&str>,
        argument_type: &dyn Fn(&Expr) -> Type<'db>,
    ) {
        let reader = OptionReader {
            db: self.db(),
            scope: self.scope,
            argument_type,
        };
        if let Some(problems) =
            property_problems(&reader, argument_type(&call.func), call, data_block_owner)
        {
            self.report_invalid(problems);
        }
    }

    /// An annotation outside a class body that is a call: `SHARED: EnumProperty(...)`. Blender
    /// never registers it, but the owner-free checks run wherever a call is written.
    fn check_annotation(&self, assign: &ast::StmtAnnAssign) {
        let Expr::Call(call) = &*assign.annotation else {
            return;
        };
        let Some(definition) = self.index.try_definition(assign) else {
            return;
        };
        let (db, scope) = (self.db(), self.scope);
        let argument_type = |expression: &Expr| {
            definition_expression_type_in_scope(db, definition, expression, scope)
        };
        self.check_call_with(call, None, &argument_type);
    }

    /// An annotated member of a class body: `value: IntProperty()`.
    fn check_member(&self, class: StaticClassLiteral<'db>, assign: &ast::StmtAnnAssign) {
        let db = self.db();
        let Expr::Name(target) = &*assign.target else {
            return;
        };
        let Some(definition) = self.index.try_definition(assign) else {
            return;
        };
        let is_property = || property_value(db, definition).is_some();
        if is_ignored_host(db, class) {
            if is_property() {
                let host = bpy_types_bases(db, class).next().unwrap_or("bpy_struct");
                self.report(
                    &BLENDER_UNSUPPORTED_PROPERTY_HOST,
                    Problem::new(
                        target,
                        format!(
                            "Blender ignores property annotations on a `{host}`: `{}` does not become a property",
                            target.id
                        ),
                    ),
                );
            }
            return;
        }
        if target.id.starts_with('_') && is_property() {
            self.report_invalid(vec![underscore_name(target, &target.id)]);
        }
        let scope = self.scope;
        let argument_type = |expression: &Expr| {
            definition_expression_type_in_scope(db, definition, expression, scope)
        };
        let reader = OptionReader {
            db,
            scope,
            argument_type: &argument_type,
        };
        let data_block_owner = forbids_data_blocks(db, class).then(|| class.name(db).as_str());
        let problems = with_member_annotation(db, definition, self.module, |annotation, quoted| {
            let Expr::Call(call) = annotation else {
                return None;
            };
            let mut problems =
                property_problems(&reader, argument_type(&call.func), call, data_block_owner)?;
            // The ranges of a quoted annotation's parts are within the string, not the file.
            if let Some(string) = quoted {
                for problem in &mut problems {
                    problem.range = string;
                }
            }
            Some(problems)
        });
        self.report_invalid(problems.unwrap_or_default());
    }

    /// `x = IntProperty()` in the body of a class with a `bpy.types` base.
    fn check_legacy(&self, assign: &ast::StmtAssign) -> bool {
        let Some(class) = self.class else {
            return false;
        };
        let Expr::Call(call) = &*assign.value else {
            return false;
        };
        if bpy_types_bases(self.db(), class).next().is_none() || !self.is_property_call(call) {
            return false;
        }
        for target in &assign.targets {
            if let Expr::Name(name) = target {
                self.report(
                    &BLENDER_LEGACY_PROPERTY,
                    Problem::new(
                        name,
                        format!(
                            "Blender only reads annotations: `{0} = ...` declares no property, write `{0}: ...`",
                            name.id
                        ),
                    ),
                );
            }
        }
        true
    }

    /// The registration whose owner is `owner`, as the Add-on index found it.
    fn registration(&self, owner: &Expr) -> Option<&'db RegisteredProperty<'db>> {
        self.registrations
            .iter()
            .find(|registration| registration.owner_range == owner.range())
    }

    /// `owner.name = value` or `setattr(owner, "name", value)`, a registration of the property
    /// `name`, which is written at `name_range`.
    fn check_registration(
        &mut self,
        registration: &RegisteredProperty<'db>,
        name: &str,
        name_range: TextRange,
        value: &Expr,
    ) {
        let db = self.db();
        let owner = registration.owner.name(db).as_str();
        if !registration.accepted {
            self.report(
                &BLENDER_UNSUPPORTED_PROPERTY_TARGET,
                Problem::new(
                    registration.owner_range,
                    format!(
                        "`{owner}` does not accept Registered properties: Blender keeps `{name}` as a plain class attribute and creates no property"
                    ),
                ),
            );
            return;
        }
        if name.starts_with('_') {
            self.report_invalid(vec![underscore_name(name_range, name)]);
        }
        let Expr::Call(call) = value else {
            self.visit_expr(value);
            return;
        };
        self.check_call(
            call,
            forbids_data_blocks(db, registration.owner).then_some(owner),
        );
        walk_expr(self, value);
    }

    /// `setattr(owner, "name", value)` when it registers a property; whether it did.
    fn check_setattr(&mut self, call: &ast::ExprCall) -> bool {
        if let Expr::Name(function) = &*call.func
            && function.id.as_str() == "setattr"
            && call.arguments.keywords.is_empty()
            && let [owner, Expr::StringLiteral(name), value] = &*call.arguments.args
            && let Some(registration) = self.registration(owner)
        {
            self.check_registration(registration, name.value.to_str(), name.range(), value);
            return true;
        }
        false
    }
}

impl<'ast> Visitor<'ast> for Checker<'_, '_, '_> {
    fn visit_stmt(&mut self, statement: &'ast ast::Stmt) {
        match statement {
            ast::Stmt::ClassDef(class_def) => {
                let scope = self.node_scope(NodeWithScopeRef::Class(class_def));
                let class = nearest_enclosing_class(self.db(), self.index, scope);
                self.visit_scope(scope, class, &class_def.body);
            }
            ast::Stmt::FunctionDef(function) => {
                let scope = self.node_scope(NodeWithScopeRef::Function(function));
                self.visit_scope(scope, None, &function.body);
            }
            ast::Stmt::AnnAssign(assign) => {
                match self.class {
                    Some(class) => self.check_member(class, assign),
                    None => self.check_annotation(assign),
                }
                if let Some(value) = &assign.value {
                    self.visit_expr(value);
                }
            }
            ast::Stmt::Assign(assign) => {
                if self.check_legacy(assign) {
                    return;
                }
                let registration = assign.targets.iter().find_map(|target| {
                    let target = target.as_attribute_expr()?;
                    Some((self.registration(&target.value)?, target))
                });
                match registration {
                    Some((registration, target)) => self.check_registration(
                        registration,
                        target.attr.id.as_str(),
                        target.attr.range(),
                        &assign.value,
                    ),
                    None => walk_stmt(self, statement),
                }
            }
            ast::Stmt::Expr(expression) => {
                if !(expression
                    .value
                    .as_call_expr()
                    .is_some_and(|call| self.check_setattr(call)))
                {
                    walk_stmt(self, statement);
                }
            }
            statement => walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'ast Expr) {
        match expression {
            // Their own scopes: `lambda: IntProperty(...)`, `[IntProperty(...) for _ in ...]`.
            Expr::Lambda(lambda) => {
                // The defaults belong to the enclosing scope.
                for parameter in lambda
                    .parameters
                    .iter()
                    .flat_map(|parameters| parameters.iter_non_variadic_params())
                {
                    if let Some(default) = parameter.default() {
                        self.visit_expr(default);
                    }
                }
                let scope = self.node_scope(NodeWithScopeRef::Lambda(lambda));
                self.in_scope(scope, None, |checker| checker.visit_expr(&lambda.body));
            }
            Expr::ListComp(comprehension) => self.visit_comprehension(
                NodeWithScopeRef::ListComprehension(comprehension),
                &comprehension.generators,
                &[&comprehension.elt],
            ),
            Expr::SetComp(comprehension) => self.visit_comprehension(
                NodeWithScopeRef::SetComprehension(comprehension),
                &comprehension.generators,
                &[&comprehension.elt],
            ),
            Expr::Generator(generator) => self.visit_comprehension(
                NodeWithScopeRef::GeneratorExpression(generator),
                &generator.generators,
                &[&generator.elt],
            ),
            Expr::DictComp(comprehension) => self.visit_comprehension(
                NodeWithScopeRef::DictComprehension(comprehension),
                &comprehension.generators,
                &comprehension
                    .key
                    .as_deref()
                    .into_iter()
                    .chain([&*comprehension.value])
                    .collect::<Vec<_>>(),
            ),
            Expr::Call(call) => {
                self.check_call(call, None);
                walk_expr(self, expression);
            }
            expression => walk_expr(self, expression),
        }
    }
}

/// Whether Blender rejects a pointer to a data-block (an ID) among the properties of `class`, as an
/// annotation or as a Registered property.
///
/// These are the structs Blender flags `STRUCT_NO_DATABLOCK_IDPROPERTIES`, and the Property hosts
/// whose properties are stored in one of them: an Operator's and a Macro's in `OperatorProperties`,
/// a Gizmo's in `GizmoProperties`.
fn forbids_data_blocks<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    const OWNERS: [&str; 12] = [
        "AddonPreferences",
        "AssetMetaData",
        "Gizmo",
        "GizmoGroupProperties",
        "GizmoProperties",
        "IDPropertyWrapPtr",
        "KeyConfigPreferences",
        "Macro",
        "Operator",
        "OperatorProperties",
        "UIList",
        "UndoStep",
    ];
    bpy_types_bases(db, class).any(|base| OWNERS.contains(&base))
}

/// The problem with a property named `_x`.
fn underscore_name(range: impl Ranged, name: &str) -> Problem {
    Problem::new(
        range,
        format!("Blender rejects property names that start with `_`: `{name}`"),
    )
}

/// What Blender rejects in the `bpy.props` call `call`, whose function has the type `callee`; `None`
/// when it is not a call of a `bpy.props` function. `data_block_owner` names the class that owns the
/// declaration when it cannot hold pointers to data-blocks (`AddonPreferences`).
///
/// An option the checker cannot read never gives a problem.
fn property_problems<'db>(
    reader: &OptionReader<'_, 'db>,
    callee: Type<'db>,
    call: &ast::ExprCall,
    data_block_owner: Option<&str>,
) -> Option<Vec<Problem>> {
    let declaration = PropertyDeclaration::from_call(reader, callee, call)?;
    let keyword = |name: &str| call.arguments.find_keyword(name);
    let mut problems = Vec::new();

    if let Some(set) = keyword("set")
        && !set.value.is_none_literal_expr()
        && (declaration.get == Known::Absent
            || keyword("get").is_some_and(|get| get.value.is_none_literal_expr()))
    {
        problems.push(Problem::new(
            set,
            "Blender rejects `set=` without `get=`: a property with a setter needs a getter",
        ));
    }

    match declaration.kind {
        PropertyKind::Enum => {
            enum_problems(reader, &declaration, call, &mut problems);
        }
        PropertyKind::BoolVector | PropertyKind::FloatVector | PropertyKind::IntVector => {
            vector_problems(reader, &declaration, call, &mut problems);
        }
        PropertyKind::Pointer => {
            if let Some(owner) = data_block_owner
                && let Some((target, PointerTarget::Id)) = declaration.target_class(reader.db)
            {
                let range = keyword("type").map_or(call.range(), Ranged::range);
                problems.push(Problem::new(
                    range,
                    format!(
                        "`{owner}` cannot hold pointers to data-blocks such as `{}`",
                        target.name(reader.db)
                    ),
                ));
            }
        }
        _ => {}
    }
    Some(problems)
}

/// An enum's `default=`, as far as the checker reads it.
enum EnumDefault {
    Identifier(Name),
    Number(i64),
    /// A set display of string literals.
    Set(Vec<Name>),
}

impl EnumDefault {
    fn read(reader: &OptionReader<'_, '_>, default: &Expr) -> Option<Self> {
        match default {
            Expr::StringLiteral(string) => {
                Some(EnumDefault::Identifier(Name::new(string.value.to_str())))
            }
            Expr::Set(set) => set
                .elts
                .iter()
                .map(|element| Some(Name::new(element.as_string_literal_expr()?.value.to_str())))
                .collect::<Option<_>>()
                .map(EnumDefault::Set),
            default => {
                if let Some(number) = int_literal(default) {
                    return Some(EnumDefault::Number(number));
                }
                let ty = (reader.argument_type)(default);
                if let Some(number) = int_type(ty) {
                    return Some(EnumDefault::Number(number));
                }
                let string = ty.as_literal_value()?.as_string()?;
                Some(EnumDefault::Identifier(Name::new(string.value(reader.db))))
            }
        }
    }
}

fn enum_problems(
    reader: &OptionReader<'_, '_>,
    declaration: &PropertyDeclaration<'_>,
    call: &ast::ExprCall,
    problems: &mut Vec<Problem>,
) {
    let Some(options) = declaration.options.clone().or_default(Box::default()) else {
        return;
    };
    let flag = options.iter().any(|option| option == "ENUM_FLAG");
    let default = call
        .arguments
        .find_keyword("default")
        .and_then(|keyword| Some((keyword, EnumDefault::read(reader, &keyword.value)?)));
    match &declaration.items {
        Known::Value(EnumItems::Static(items)) => {
            if flag
                && items.length > 32
                && let Some(keyword) = call.arguments.find_keyword("items")
            {
                problems.push(Problem::new(
                    keyword,
                    format!(
                        "Blender allows at most 32 items with `ENUM_FLAG`, these are {}",
                        items.length
                    ),
                ));
            }
            if let Some((keyword, default)) = default
                && let Some(message) = static_default_problem(items, flag, &default)
            {
                problems.push(Problem::new(keyword, message));
            }
        }
        Known::Value(EnumItems::Function) => {
            if !flag && let Some((keyword, EnumDefault::Identifier(_))) = default {
                problems.push(Problem::new(
                    keyword,
                    "With function `items=`, Blender needs the default as an item's number, not its identifier",
                ));
            }
        }
        Known::Absent | Known::Unknown => {}
    }
}

/// What is wrong with `default` for the static enum `items`, which has `ENUM_FLAG` when `flag`.
fn static_default_problem(
    items: &StaticItems,
    flag: bool,
    default: &EnumDefault,
) -> Option<String> {
    let has_identifier = |identifier: &Name| {
        items
            .items
            .iter()
            .any(|item| &item.identifier == identifier)
    };
    match (flag, default) {
        (true, EnumDefault::Identifier(_) | EnumDefault::Number(_)) => Some(
            "With `ENUM_FLAG`, Blender needs the default as a set of identifiers".to_string(),
        ),
        (true, EnumDefault::Set(members)) => members
            .iter()
            .find(|member| !has_identifier(member))
            .map(|member| format!("`\"{member}\"` is not one of the enum's items")),
        (false, EnumDefault::Set(_)) => Some(
            "A set default needs `options={\"ENUM_FLAG\"}`; without it, Blender needs one identifier or number"
                .to_string(),
        ),
        (false, EnumDefault::Identifier(identifier)) => (!has_identifier(identifier))
            .then(|| format!("`\"{identifier}\"` is not one of the enum's items")),
        (false, EnumDefault::Number(number)) => {
            let numbers = items
                .items
                .iter()
                .map(|item| item.number)
                .collect::<Option<Vec<_>>>()?;
            (!numbers.contains(number))
                .then(|| format!("No item of the enum has the number {number}"))
        }
    }
}

fn vector_problems(
    reader: &OptionReader<'_, '_>,
    declaration: &PropertyDeclaration<'_>,
    call: &ast::ExprCall,
    problems: &mut Vec<Problem>,
) {
    if let Some(keyword) = call.arguments.find_keyword("size")
        && let Some(dimensions) = read_dimensions(reader.db, &keyword.value, reader.argument_type)
        && valid_size(&dimensions).is_none()
    {
        problems.push(Problem::new(
            keyword,
            "Blender needs `size` from 1 to 32, or a tuple of up to three such dimensions",
        ));
    }
    let Some(size) = declaration.size.clone().or_default(Box::new([3])) else {
        return;
    };
    if let Some(keyword) = call.arguments.find_keyword("default")
        && matches!(keyword.value, Expr::Tuple(_) | Expr::List(_))
        && shape_matches(&keyword.value, &size) == Some(false)
    {
        let shape = match &*size {
            [length] => format!("{length} items"),
            dimensions => format!(
                "the shape ({})",
                dimensions
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        problems.push(Problem::new(
            keyword,
            format!("Blender needs a default of {shape} to match `size`"),
        ));
    }
}

/// Whether the display `expression` is nested as `dimensions` says, `None` when some part of it
/// cannot be read (a name, a call, an unpacked `*sequence`).
fn shape_matches(expression: &Expr, dimensions: &[u8]) -> Option<bool> {
    match expression {
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
            // A sequence where a number belongs.
            let [length, inner @ ..] = dimensions else {
                return Some(false);
            };
            if elts.iter().any(Expr::is_starred_expr) {
                return None;
            }
            if elts.len() != usize::from(*length) {
                return Some(false);
            }
            let mut known = true;
            for element in elts {
                match shape_matches(element, inner) {
                    Some(false) => return Some(false),
                    Some(true) => {}
                    None => known = false,
                }
            }
            known.then_some(true)
        }
        Expr::NumberLiteral(_) | Expr::BooleanLiteral(_) | Expr::UnaryOp(_) => {
            Some(dimensions.is_empty())
        }
        _ => None,
    }
}
