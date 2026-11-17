//! Narrowing by a `poll()`: a pointer the class's `poll()` checked is not `None` in the callbacks the
//! poll gates.
//!
//! Blender calls a Panel's `draw`, a Menu's `draw`, an Operator's `invoke` and `execute` and a
//! GizmoGroup's setup callbacks only right after `poll()` returned true, with the same context
//! ([`ty_blender::gated_by_poll`]). So what the poll checked holds there:
//!
//! ```python
//! class MY_PT_material(Panel):
//!     @classmethod
//!     def poll(cls, context):
//!         ob = context.object
//!         return ob is not None and ob.type == 'MESH'
//!
//!     def draw(self, context):
//!         context.object.data      # `Mesh`: reported under `blender-narrowed-by-poll`
//!         context.material.name    # still an error: the poll did not check it
//! ```
//!
//! [`poll_facts`] reads what a poll checks from its code, without inference: the conjuncts of its
//! `return` (`X`, `X is not None`, `isinstance(X, T)`, `bool(X)`, `X.type == 'LIT'` / `in {...}`),
//! the attribute reads always evaluated while it computes the result (a poll that raises is a poll
//! that failed, so `ob.data.is_editmode` means `ob` and `ob.data` were set), early `return False`
//! guards, single-assignment aliases, `super().poll(context)` / `Base.poll(context)` and helper
//! functions taking the context, followed [`MAX_HELPER_DEPTH`] calls deep. A fact is about a
//! [`Path`] from the context, never about the class as a whole, so a pointer the poll did not check
//! keeps its stub type. A Type tag test types the struct as the same test in the callback would
//! ([`narrow_to_tag_values`]), and one on `context.area` makes `context.space_data` the editor's
//! subclass, since a space is its area's first.
//!
//! The read is reported at the narrowed expression, unless the callback only tests it for truth or
//! `None` ([`FunctionPaths`]).

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashMap;
use ty_blender::gated_by_poll;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::{NodeWithScopeRef, ScopeId};
use ty_python_core::{ProgramFile, semantic_index};

use crate::place::PlaceAndQualifiers;
use crate::types::context::InferContext;
use crate::types::infer::nearest_enclosing_class;
use crate::types::{StaticClassLiteral, Type, UnionType};
use crate::{Db, ProgramEnvironment};

use super::classes::{mro_classes, own_binding};
use super::context_members::{enclosing_function, space_subclass};
use super::follow::{Resolved, is_classmethod, resolve_expression};
use super::lints::BLENDER_NARROWED_BY_POLL;
use super::member::without_none;
use super::overrides::callback_root;
use super::symbols::{bpy_types_class_name, is_bpy_types_class};
use super::type_tags::narrow_to_tag_values;

/// How many helper calls deep a poll is followed: `poll` → `poll_fluid_domain(context)` →
/// `_has_domain(context)` → `_modifier(context)`.
const MAX_HELPER_DEPTH: usize = 3;

/// How many aliases (`ob = context.object`, `data = ob.data`) are followed back at most.
const MAX_ALIAS_DEPTH: usize = 8;

/// An attribute chain from the context: `context.object.active_material` is
/// `["object", "active_material"]`, also read as `bpy.context.object.active_material` or through a
/// local `ob = context.object`.
type Path = Box<[Name]>;

/// What a `poll()` that returned true says about a path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
enum Fact {
    /// The pointer is not `None`.
    NotNone(Path),
    /// The pointer's Type tag `attribute` is one of `values` (`ob.type == 'MESH'`).
    Tag {
        path: Path,
        attribute: Name,
        values: Box<[Name]>,
    },
}

impl Fact {
    fn path(&self) -> &[Name] {
        match self {
            Fact::NotNone(path) | Fact::Tag { path, .. } => path,
        }
    }
}

/// Facts that hold together; a set, kept small.
#[derive(Clone, Debug, Default)]
struct Facts(Vec<Fact>);

impl Facts {
    fn add(&mut self, fact: Fact) {
        if !self.0.contains(&fact) {
            self.0.push(fact);
        }
    }

    fn extend(&mut self, other: Facts) {
        for fact in other.0 {
            self.add(fact);
        }
    }

    fn with(&self, other: Facts) -> Facts {
        let mut facts = self.clone();
        facts.extend(other);
        facts
    }

    /// The facts both hold.
    fn intersect(&self, other: &Facts) -> Facts {
        Facts(
            self.0
                .iter()
                .filter(|fact| other.0.contains(fact))
                .cloned()
                .collect(),
        )
    }

    fn not_none(&mut self, path: Option<Vec<Name>>) {
        if let Some(path) = path.filter(|path| !path.is_empty()) {
            self.add(Fact::NotNone(path.into_boxed_slice()));
        }
    }
}

/// The type of the attribute read `attribute`, where ty found `place`, when the read is in a
/// callback that its class's `poll()` gates and the poll checked its path. Reports the read when
/// the narrowing removed a `None` or replaced `Space`, unless the callback only tests it.
pub(super) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    place: PlaceAndQualifiers<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let function = enclosing_function(db, file, attribute)?;
    let gated = gated_facts(db, function).as_ref()?;
    let paths = function_paths(db, function);
    let path = paths.attribute_path(attribute)?;
    let facts: Vec<&Fact> = gated
        .facts
        .iter()
        .filter(|fact| {
            fact.path() == path.as_slice()
                || (path == ["space_data"] && matches!(fact, Fact::Tag { path, attribute, .. } if **path == ["area"] && attribute == "type"))
        })
        .collect();
    if facts.is_empty() {
        return None;
    }
    let original = place.place.ignore_possibly_undefined()?;
    let narrowed = narrow(db, env, original, &path, &facts);
    if narrowed == original {
        return None;
    }
    let removed_none = without_none(db, original) != original;
    if (removed_none || is_space(db, env, without_none(db, original)))
        && !paths.skips(attribute.range(), &path)
        && let Some(builder) = context.report_lint(&BLENDER_NARROWED_BY_POLL, attribute)
    {
        builder.into_diagnostic(format_args!(
            "`{}` is taken as `{}` because `{}.poll()` checked it; Blender calls `{}` only after its \
             poll returned true, with the same context",
            display_path(&path),
            narrowed.display(db, env),
            gated.poll_class,
            gated.callback,
        ));
    }
    Some(place.map_type(|_| narrowed))
}

/// `ty`, the type of a read of `path`, narrowed by `facts` about it.
fn narrow<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    path: &[Name],
    facts: &[&Fact],
) -> Type<'db> {
    let mut ty = ty;
    for fact in facts {
        ty = without_none(db, ty);
        let Fact::Tag {
            path: fact_path,
            attribute,
            values,
        } = fact
        else {
            continue;
        };
        if **fact_path == *path {
            if let Some(tagged) = narrow_to_tag_values(db, env, ty, attribute, values) {
                ty = tagged;
            }
        } else if let Some(spaces) = values
            .iter()
            .map(|value| space_subclass(db, env, value))
            .collect::<Option<Vec<_>>>()
        {
            // `context.area.type == 'VIEW_3D'`: the space is the area's first.
            ty = UnionType::from_elements(db, env, spaces);
        }
    }
    ty
}

/// Whether `ty` is an instance of `bpy.types.Space` itself.
fn is_space<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> bool {
    let Type::NominalInstance(instance) = ty else {
        return false;
    };
    instance
        .class(db, env)
        .class_literal(db)
        .as_static()
        .is_some_and(|class| bpy_types_class_name(db, class) == Some("Space"))
}

fn display_path(path: &[Name]) -> String {
    let mut text = String::from("context");
    for name in path {
        text.push('.');
        text.push_str(name);
    }
    text
}

/// What a gated callback knows from its class's poll.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
struct GatedFacts {
    facts: Box<[Fact]>,
    /// The class whose `poll` is called, for the message.
    poll_class: Name,
    /// The callback's name, for the message.
    callback: Name,
}

/// The facts of the poll that gates the function scope `function`, when it is a callback the poll
/// gates ([`gated_by_poll`]) in the body of a class with a poll that checks something. The
/// callback is found by its name, as Blender looks it up.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn gated_facts<'db>(db: &'db dyn Db, function: ScopeId<'db>) -> Option<GatedFacts> {
    let file = function.program_file(db);
    let index = semantic_index(db, file);
    let in_class_body = index
        .parent_scope_id(function.file_scope_id(db))
        .is_some_and(|parent| index.scope(parent).node().as_class().is_some());
    let node = function.node(db).as_function().filter(|_| in_class_body)?;
    let module = parsed_module(db, file.python_file(db)).load(db);
    let name = node.node(&module).name.id.clone();
    let class = nearest_enclosing_class(db, index, function)?;
    let root = callback_root(db, class)?;
    if !gated_by_poll(root, &name) {
        return None;
    }
    let (facts, poll_class) = poll_facts(db, class).as_ref()?;
    if facts.is_empty() {
        return None;
    }
    Some(GatedFacts {
        facts: facts.clone(),
        poll_class: poll_class.clone(),
        callback: name,
    })
}

/// What the `poll` Blender calls for `class` checks, with the name of the class that defines it:
/// the first `poll` in its method resolution order, before the `bpy.types` classes. `None` for a
/// class without one.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn poll_facts<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> Option<(Box<[Fact]>, Name)> {
    let (definition, defining) = method_on(db, class, "poll")?;
    let facts = function_facts(db, definition, Owner::Class { class, defining }, 1, None, 0)?;
    Some((facts.0.into_boxed_slice(), defining.name(db).clone()))
}

/// The method `name` of `class`: its definition and the class whose body defines it, the first in
/// the method resolution order before the `bpy.types` classes.
fn method_on<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<(Definition<'db>, StaticClassLiteral<'db>)> {
    method_after(db, class, None, name)
}

/// The method `name` of `class` found after `after` in its method resolution order
/// (`super().name` in a method of `after`), or from the start without one.
fn method_after<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    after: Option<StaticClassLiteral<'db>>,
    name: &str,
) -> Option<(Definition<'db>, StaticClassLiteral<'db>)> {
    let mut mro = mro_classes(db, class).peekable();
    if let Some(after) = after {
        for base in mro.by_ref() {
            if base == after {
                break;
            }
        }
    }
    for base in mro {
        if is_bpy_types_class(db, base) {
            return None;
        }
        if let Some((definition, _)) = own_binding(db, base, name) {
            return matches!(definition.kind(db), DefinitionKind::Function(_))
                .then_some((definition, base));
        }
    }
    None
}

/// The class a method is read for: `class` is the class Blender calls it on, `defining` the class
/// whose body defines it (`super()` starts after it). A plain function has none.
#[derive(Copy, Clone)]
enum Owner<'db> {
    Class {
        class: StaticClassLiteral<'db>,
        defining: StaticClassLiteral<'db>,
    },
    Function,
}

/// The facts that hold whenever the function `definition` returns a true value, its context being
/// the parameter at `position` (or the one named `keyword`). `None` when it never returns one.
fn function_facts<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    owner: Owner<'db>,
    position: usize,
    keyword: Option<&Name>,
    depth: usize,
) -> Option<Facts> {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let file = definition.program_file(db);
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let function = function.node(&module);
    let parameters = &function.parameters;
    let positional: Vec<&Name> = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .map(|parameter| &parameter.parameter.name.id)
        .collect();
    let context = match keyword {
        Some(keyword) => parameters
            .iter_non_variadic_params()
            .map(|parameter| &parameter.parameter.name.id)
            .find(|name| *name == keyword)?,
        None => *positional.get(position)?,
    };
    let scope = semantic_index(db, file)
        .node_scope(NodeWithScopeRef::Function(function))
        .to_scope_id(db, file);
    let aliases = aliases(function, context);
    let reader = PollReader {
        db,
        scope,
        context,
        first: positional.first().copied(),
        aliases: &aliases,
        owner,
        depth,
    };
    let flow = reader.block(&function.body, Facts::default());
    let mut returns = flow.returns.into_iter();
    let first = returns.next()?;
    Some(returns.fold(first, |facts, other| facts.intersect(&other)))
}

/// Where the statements of a block lead.
struct Flow {
    /// The facts at the end of the block, when it can fall through.
    fallthrough: Option<Facts>,
    /// The facts at each `return` that may give a true value.
    returns: Vec<Facts>,
}

struct PollReader<'db, 'a> {
    db: &'db dyn Db,
    /// The function's body scope, where its names are resolved.
    scope: ScopeId<'db>,
    context: &'a Name,
    /// The first parameter, `cls` or `self` in a method.
    first: Option<&'a Name>,
    aliases: &'a FxHashMap<Name, Alias>,
    owner: Owner<'db>,
    depth: usize,
}

impl<'db> PollReader<'db, '_> {
    fn path(&self, expression: &Expr) -> Option<Vec<Name>> {
        path_of(expression, self.context, self.aliases)
    }

    fn block(&self, body: &[Stmt], facts: Facts) -> Flow {
        let mut facts = facts;
        let mut returns = Vec::new();
        for statement in body {
            match statement {
                Stmt::Return(ast::StmtReturn { value, .. }) => {
                    if let Some(value) = value.as_deref()
                        && !is_false(value)
                    {
                        returns.push(facts.with(self.truthy(value)));
                    }
                    return Flow {
                        fallthrough: None,
                        returns,
                    };
                }
                Stmt::Raise(_) => {
                    return Flow {
                        fallthrough: None,
                        returns,
                    };
                }
                Stmt::If(if_statement) => {
                    let mut ends: Vec<Facts> = Vec::new();
                    let mut negated = facts.clone();
                    let body = self.block(
                        &if_statement.body,
                        negated.with(self.truthy(&if_statement.test)),
                    );
                    returns.extend(body.returns);
                    ends.extend(body.fallthrough);
                    negated.extend(self.falsy(&if_statement.test));
                    let mut has_else = false;
                    for clause in &if_statement.elif_else_clauses {
                        let arm = match &clause.test {
                            Some(test) => {
                                let arm = self.block(&clause.body, negated.with(self.truthy(test)));
                                negated.extend(self.falsy(test));
                                arm
                            }
                            None => {
                                has_else = true;
                                self.block(&clause.body, negated.clone())
                            }
                        };
                        returns.extend(arm.returns);
                        ends.extend(arm.fallthrough);
                    }
                    if !has_else {
                        ends.push(negated);
                    }
                    let mut ends = ends.into_iter();
                    let Some(first) = ends.next() else {
                        return Flow {
                            fallthrough: None,
                            returns,
                        };
                    };
                    facts = ends.fold(first, |facts, other| facts.intersect(&other));
                }
                Stmt::Expr(ast::StmtExpr { value, .. }) => facts.extend(self.reads(value)),
                Stmt::Assign(ast::StmtAssign { value, .. })
                | Stmt::AugAssign(ast::StmtAugAssign { value, .. }) => {
                    facts.extend(self.reads(value));
                }
                Stmt::AnnAssign(ast::StmtAnnAssign {
                    value: Some(value), ..
                }) => facts.extend(self.reads(value)),
                Stmt::FunctionDef(_)
                | Stmt::ClassDef(_)
                | Stmt::Pass(_)
                | Stmt::Import(_)
                | Stmt::ImportFrom(_)
                | Stmt::AnnAssign(_)
                | Stmt::Global(_)
                | Stmt::Nonlocal(_)
                | Stmt::Delete(_)
                | Stmt::TypeAlias(_) => {}
                // A loop, `with`, `try` or `match` is not followed: a return inside it keeps only
                // what was known before it.
                _ => {
                    if may_return_true(std::slice::from_ref(statement)) {
                        returns.push(facts.clone());
                    }
                }
            }
        }
        Flow {
            fallthrough: Some(facts),
            returns,
        }
    }

    /// The facts when `test` gives a true value.
    fn truthy(&self, test: &Expr) -> Facts {
        let mut facts = Facts::default();
        match test {
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::And,
                values,
                ..
            }) => {
                for value in values {
                    facts.extend(self.truthy(value));
                }
            }
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::Or,
                values,
                ..
            }) => {
                let mut arms = values.iter().map(|value| self.truthy(value));
                if let Some(first) = arms.next() {
                    facts.extend(arms.fold(first, |facts, other| facts.intersect(&other)));
                }
                if let Some(first) = values.first() {
                    facts.extend(self.reads(first));
                }
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => facts.extend(self.falsy(operand)),
            Expr::Compare(compare) => {
                facts.extend(self.reads(test));
                if let Some((operand, op, other)) = single_comparison(compare) {
                    match op {
                        ast::CmpOp::IsNot | ast::CmpOp::NotEq if other.is_none_literal_expr() => {
                            facts.not_none(self.path(operand));
                        }
                        ast::CmpOp::Eq | ast::CmpOp::In => facts.extend(self.tag(operand, other)),
                        _ => {}
                    }
                }
            }
            Expr::Call(call) => {
                facts.extend(self.reads(test));
                if let Some(checked) = checked_argument(call) {
                    facts.not_none(self.path(checked));
                } else if let Some(helper) = self.call_facts(call) {
                    facts.extend(helper);
                }
            }
            Expr::Name(_) | Expr::Attribute(_) => {
                facts.extend(self.reads(test));
                facts.not_none(self.path(test));
            }
            _ => facts.extend(self.reads(test)),
        }
        facts
    }

    /// The facts when `test` gives a false value.
    fn falsy(&self, test: &Expr) -> Facts {
        let mut facts = Facts::default();
        match test {
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::Or,
                values,
                ..
            }) => {
                for value in values {
                    facts.extend(self.falsy(value));
                }
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => facts.extend(self.truthy(operand)),
            Expr::Compare(compare) => {
                facts.extend(self.reads(test));
                if let Some((operand, op, other)) = single_comparison(compare) {
                    match op {
                        ast::CmpOp::Is | ast::CmpOp::Eq if other.is_none_literal_expr() => {
                            facts.not_none(self.path(operand));
                        }
                        ast::CmpOp::NotEq | ast::CmpOp::NotIn => {
                            facts.extend(self.tag(operand, other));
                        }
                        _ => {}
                    }
                }
            }
            _ => facts.extend(self.reads(test)),
        }
        facts
    }

    /// The facts of `<struct>.<attribute> == <literal>` (or `in` a literal collection) when it
    /// holds: the struct is not `None`, and its tag `attribute` is one of the literals.
    fn tag(&self, operand: &Expr, other: &Expr) -> Facts {
        let mut facts = Facts::default();
        let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = operand else {
            return facts;
        };
        let Some(values) = string_literals(other) else {
            return facts;
        };
        if let Some(path) = self.path(value).filter(|path| !path.is_empty()) {
            facts.add(Fact::Tag {
                path: path.into_boxed_slice(),
                attribute: attr.id.clone(),
                values: values.into_boxed_slice(),
            });
        }
        facts
    }

    /// The attribute reads always evaluated while `expression` is: each makes its receiver not
    /// `None`, since a poll that raises fails. Only the first operand of `and` / `or` and the test
    /// of a conditional expression are always evaluated; lambdas and comprehensions are skipped.
    fn reads(&self, expression: &Expr) -> Facts {
        let mut facts = Facts::default();
        self.collect_reads(expression, &mut facts);
        facts
    }

    fn collect_reads(&self, expression: &Expr, facts: &mut Facts) {
        match expression {
            Expr::Attribute(ast::ExprAttribute { value, .. }) => {
                facts.not_none(self.path(value));
                self.collect_reads(value, facts);
            }
            Expr::Call(call) => {
                self.collect_reads(&call.func, facts);
                for argument in &call.arguments.args {
                    self.collect_reads(argument, facts);
                }
                for keyword in &call.arguments.keywords {
                    self.collect_reads(&keyword.value, facts);
                }
            }
            Expr::Compare(compare) => {
                for operand in compare.operands.iter().take(2) {
                    self.collect_reads(operand, facts);
                }
            }
            Expr::BoolOp(ast::ExprBoolOp { values, .. }) => {
                if let Some(first) = values.first() {
                    self.collect_reads(first, facts);
                }
            }
            Expr::BinOp(ast::ExprBinOp { left, right, .. }) => {
                self.collect_reads(left, facts);
                self.collect_reads(right, facts);
            }
            Expr::UnaryOp(ast::ExprUnaryOp { operand, .. }) => self.collect_reads(operand, facts),
            Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
                self.collect_reads(value, facts);
                self.collect_reads(slice, facts);
            }
            Expr::If(ast::ExprIf { test, .. }) => self.collect_reads(test, facts),
            Expr::Tuple(ast::ExprTuple { elts, .. })
            | Expr::List(ast::ExprList { elts, .. })
            | Expr::Set(ast::ExprSet { elts, .. }) => {
                for element in elts {
                    self.collect_reads(element, facts);
                }
            }
            Expr::Starred(ast::ExprStarred { value, .. }) => self.collect_reads(value, facts),
            _ => {}
        }
    }

    /// The facts of a helper `call` that is passed the context: a function (`helper(context)`,
    /// `module.helper(context)`), a method of the class (`cls._poll(context)`), of a named class
    /// (`Base.poll(context)`) or of the next class (`super().poll(context)`).
    fn call_facts(&self, call: &ast::ExprCall) -> Option<Facts> {
        if self.depth >= MAX_HELPER_DEPTH {
            return None;
        }
        let is_context =
            |expression: &Expr| self.path(expression).is_some_and(|path| path.is_empty());
        let position = call.arguments.args.iter().position(is_context);
        let keyword = call
            .arguments
            .keywords
            .iter()
            .find(|keyword| is_context(&keyword.value))
            .and_then(|keyword| keyword.arg.as_ref())
            .map(|keyword| keyword.id.clone());
        if position.is_none() && keyword.is_none() {
            return None;
        }
        let (definition, owner, bound) = self.callee(&call.func)?;
        let (position, keyword) = match position {
            Some(position) => (position + usize::from(bound), None),
            None => (0, keyword.as_ref()),
        };
        function_facts(
            self.db,
            definition,
            owner,
            position,
            keyword,
            self.depth + 1,
        )
    }

    /// The function `callee` calls, the class it is read for, and whether the call binds its first
    /// parameter (a method read from a class or instance, a classmethod).
    fn callee(&self, callee: &Expr) -> Option<(Definition<'db>, Owner<'db>, bool)> {
        let db = self.db;
        if let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = callee {
            let name = attr.id.as_str();
            // `super().poll(context)`
            if let Expr::Call(ast::ExprCall {
                func, arguments, ..
            }) = &**value
                && func.as_name_expr().is_some_and(|func| func.id == "super")
                && arguments.is_empty()
            {
                let Owner::Class { class, defining } = self.owner else {
                    return None;
                };
                let (definition, defining) = method_after(db, class, Some(defining), name)?;
                return Some((definition, Owner::Class { class, defining }, true));
            }
            // `cls._poll(context)`, `self._poll(context)`
            if let Expr::Name(receiver) = &**value
                && self.first == Some(&receiver.id)
                && let Owner::Class { class, .. } = self.owner
            {
                let (definition, defining) = method_on(db, class, name)?;
                let bound = !is_staticmethod(db, definition);
                return Some((definition, Owner::Class { class, defining }, bound));
            }
            // `Base.poll(context)`
            if let Some(Resolved::Value(Type::ClassLiteral(class))) =
                resolve_expression(db, self.scope, value)
                && let Some(class) = class.as_static()
            {
                let (definition, defining) = method_on(db, class, name)?;
                let bound = is_classmethod_definition(db, definition);
                return Some((definition, Owner::Class { class, defining }, bound));
            }
        }
        match resolve_expression(db, self.scope, callee)? {
            Resolved::Function(definition) => Some((definition, Owner::Function, false)),
            _ => None,
        }
    }
}

fn is_staticmethod(db: &dyn Db, definition: Definition<'_>) -> bool {
    has_decorator(db, definition, "staticmethod")
}

fn is_classmethod_definition(db: &dyn Db, definition: Definition<'_>) -> bool {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return false;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    is_classmethod(function.node(&module))
}

fn has_decorator(db: &dyn Db, definition: Definition<'_>, name: &str) -> bool {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return false;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    function
        .node(&module)
        .decorator_list
        .iter()
        .any(|decorator| {
            decorator
                .expression
                .as_name_expr()
                .is_some_and(|decorator| decorator.id == name)
        })
}

/// Whether `expression` is a literal false value: `False`, `None` or `0`.
fn is_false(expression: &Expr) -> bool {
    match expression {
        Expr::BooleanLiteral(literal) => !literal.value,
        Expr::NoneLiteral(_) => true,
        Expr::NumberLiteral(ast::ExprNumberLiteral {
            value: ast::Number::Int(value),
            ..
        }) => *value == 0,
        _ => false,
    }
}

/// Whether `body` holds a `return` of anything but a literal false value, outside nested
/// functions and classes.
fn may_return_true(body: &[Stmt]) -> bool {
    body.iter().any(|statement| match statement {
        Stmt::Return(ast::StmtReturn { value, .. }) => {
            value.as_deref().is_some_and(|value| !is_false(value))
        }
        Stmt::If(statement) => {
            may_return_true(&statement.body)
                || statement
                    .elif_else_clauses
                    .iter()
                    .any(|clause| may_return_true(&clause.body))
        }
        Stmt::For(statement) => {
            may_return_true(&statement.body) || may_return_true(&statement.orelse)
        }
        Stmt::While(statement) => {
            may_return_true(&statement.body) || may_return_true(&statement.orelse)
        }
        Stmt::With(statement) => may_return_true(&statement.body),
        Stmt::Try(statement) => {
            may_return_true(&statement.body)
                || may_return_true(&statement.orelse)
                || may_return_true(&statement.finalbody)
                || statement.handlers.iter().any(|handler| {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    may_return_true(&handler.body)
                })
        }
        Stmt::Match(statement) => statement
            .cases
            .iter()
            .any(|case| may_return_true(&case.body)),
        _ => false,
    })
}

/// The operand, operator and other side of a comparison with one operator, with the `None` literal
/// put on the right: `None is X` is read as `X is None`.
fn single_comparison(compare: &ast::ExprCompare) -> Option<(&Expr, ast::CmpOp, &Expr)> {
    let (left, op, right) = compare.as_single()?;
    if left.is_none_literal_expr()
        && matches!(
            op,
            ast::CmpOp::Is | ast::CmpOp::IsNot | ast::CmpOp::Eq | ast::CmpOp::NotEq
        )
    {
        return Some((right, *op, left));
    }
    Some((left, *op, right))
}

/// The argument of `isinstance(X, T)` or `bool(X)`, which is true only when `X` is not `None`.
fn checked_argument(call: &ast::ExprCall) -> Option<&Expr> {
    let Expr::Name(function) = &*call.func else {
        return None;
    };
    match (function.id.as_str(), &*call.arguments.args) {
        ("isinstance", [checked, _]) | ("bool", [checked])
            if call.arguments.keywords.is_empty() =>
        {
            Some(checked)
        }
        _ => None,
    }
}

/// The values of a string literal, or of a tuple, list or set display of them.
fn string_literals(expression: &Expr) -> Option<Vec<Name>> {
    match expression {
        Expr::StringLiteral(literal) => Some(vec![Name::new(literal.value.to_str())]),
        Expr::Tuple(ast::ExprTuple { elts, .. })
        | Expr::List(ast::ExprList { elts, .. })
        | Expr::Set(ast::ExprSet { elts, .. }) => elts
            .iter()
            .map(|element| match element {
                Expr::StringLiteral(literal) => Some(Name::new(literal.value.to_str())),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// A local name assigned once, to a value that is a path from the context.
#[derive(Clone, Debug)]
struct Alias {
    path: Vec<Name>,
    /// The range of the assigned value.
    value: TextRange,
}

/// The path of `expression` from the context `context`: the context itself (`context`,
/// `bpy.context`) is the empty path, and an attribute read extends its receiver's.
fn path_of(
    expression: &Expr,
    context: &Name,
    aliases: &FxHashMap<Name, Alias>,
) -> Option<Vec<Name>> {
    match expression {
        Expr::Name(name) if name.id == *context => Some(Vec::new()),
        Expr::Name(name) => aliases.get(&name.id).map(|alias| alias.path.clone()),
        Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            if attr.id == "context" && value.as_name_expr().is_some_and(|value| value.id == "bpy") {
                return Some(Vec::new());
            }
            let mut path = path_of(value, context, aliases)?;
            path.push(attr.id.clone());
            Some(path)
        }
        _ => None,
    }
}

/// The names of `function` assigned exactly once, by `name = value`, to a path from `context`. A
/// name bound any other way (a parameter, a loop target, an augmented assignment, ...) or twice is
/// no alias.
fn aliases(function: &ast::StmtFunctionDef, context: &Name) -> FxHashMap<Name, Alias> {
    let mut bindings = Bindings::default();
    for parameter in function.parameters.iter() {
        bindings.other(parameter.name().id.clone());
    }
    bindings.block(&function.body);
    let values: FxHashMap<Name, &Expr> = bindings
        .assigned
        .into_iter()
        .filter(|(name, values)| values.len() == 1 && !bindings.others.contains(name))
        .filter_map(|(name, values)| Some((name, *values.first()?)))
        .collect();
    let mut aliases = FxHashMap::default();
    for (name, value) in &values {
        if let Some(path) = alias_path(value, context, &values, 0) {
            aliases.insert(
                name.clone(),
                Alias {
                    path,
                    value: value.range(),
                },
            );
        }
    }
    aliases
}

fn alias_path(
    expression: &Expr,
    context: &Name,
    values: &FxHashMap<Name, &Expr>,
    depth: usize,
) -> Option<Vec<Name>> {
    if depth > MAX_ALIAS_DEPTH {
        return None;
    }
    match expression {
        Expr::Name(name) if name.id == *context => Some(Vec::new()),
        Expr::Name(name) => alias_path(values.get(&name.id)?, context, values, depth + 1),
        Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            if attr.id == "context" && value.as_name_expr().is_some_and(|value| value.id == "bpy") {
                return Some(Vec::new());
            }
            let mut path = alias_path(value, context, values, depth)?;
            path.push(attr.id.clone());
            Some(path)
        }
        _ => None,
    }
}

/// The bindings of a function body, outside nested functions and classes.
#[derive(Default)]
struct Bindings<'a> {
    assigned: FxHashMap<Name, Vec<&'a Expr>>,
    others: Vec<Name>,
}

impl<'a> Bindings<'a> {
    fn other(&mut self, name: Name) {
        self.others.push(name);
    }

    fn target(&mut self, target: &Expr) {
        match target {
            Expr::Name(name) => self.other(name.id.clone()),
            Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
                for element in elts {
                    self.target(element);
                }
            }
            Expr::Starred(ast::ExprStarred { value, .. }) => self.target(value),
            _ => {}
        }
    }

    fn block(&mut self, body: &'a [Stmt]) {
        for statement in body {
            match statement {
                Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
                    for target in targets {
                        if let Expr::Name(name) = target {
                            self.assigned
                                .entry(name.id.clone())
                                .or_default()
                                .push(value);
                        } else {
                            self.target(target);
                        }
                    }
                }
                Stmt::AnnAssign(ast::StmtAnnAssign { target, value, .. }) => {
                    match (&**target, value) {
                        (Expr::Name(name), Some(value)) => {
                            self.assigned
                                .entry(name.id.clone())
                                .or_default()
                                .push(value);
                        }
                        _ => self.target(target),
                    }
                }
                Stmt::AugAssign(ast::StmtAugAssign { target, .. }) => self.target(target),
                Stmt::Delete(ast::StmtDelete { targets, .. }) => {
                    for target in targets {
                        self.target(target);
                    }
                }
                Stmt::For(statement) => {
                    self.target(&statement.target);
                    self.block(&statement.body);
                    self.block(&statement.orelse);
                }
                Stmt::While(statement) => {
                    self.block(&statement.body);
                    self.block(&statement.orelse);
                }
                Stmt::If(statement) => {
                    self.block(&statement.body);
                    for clause in &statement.elif_else_clauses {
                        self.block(&clause.body);
                    }
                }
                Stmt::With(statement) => {
                    for item in &statement.items {
                        if let Some(target) = &item.optional_vars {
                            self.target(target);
                        }
                    }
                    self.block(&statement.body);
                }
                Stmt::Try(statement) => {
                    self.block(&statement.body);
                    for handler in &statement.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        if let Some(name) = &handler.name {
                            self.other(name.id.clone());
                        }
                        self.block(&handler.body);
                    }
                    self.block(&statement.orelse);
                    self.block(&statement.finalbody);
                }
                Stmt::Match(statement) => {
                    for case in &statement.cases {
                        self.block(&case.body);
                    }
                }
                Stmt::FunctionDef(ast::StmtFunctionDef { name, .. })
                | Stmt::ClassDef(ast::StmtClassDef { name, .. }) => self.other(name.id.clone()),
                Stmt::Import(ast::StmtImport { names, .. })
                | Stmt::ImportFrom(ast::StmtImportFrom { names, .. }) => {
                    for alias in names {
                        let name = alias.asname.as_ref().unwrap_or(&alias.name);
                        self.other(name.id.clone());
                    }
                }
                Stmt::Global(ast::StmtGlobal { names, .. })
                | Stmt::Nonlocal(ast::StmtNonlocal { names, .. }) => {
                    for name in names {
                        self.other(name.id.clone());
                    }
                }
                _ => {}
            }
        }
    }
}

/// The paths from the context in a Blender callback, and the reads it only tests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) struct FunctionPaths {
    /// The callback's context parameter (the second one).
    context: Option<Name>,
    /// The single-assignment aliases of paths.
    aliases: Box<[(Name, Path)]>,
    /// The reads in a test for truth or `None` (`if context.object:`, `x is None`), also the value
    /// of an alias that is tested so (`ob = context.object` with `if ob is None:` after it).
    tested: Box<[TextRange]>,
    /// Where the callback's own test says a path is not `None`: the body of `if X:` or `if X is
    /// not None:`, and the rest of the block after `if not X: return` or `assert X`.
    regions: Box<[(Path, TextRange)]>,
}

impl FunctionPaths {
    /// The path of the attribute read `attribute` in the callback.
    fn attribute_path(&self, attribute: &ast::ExprAttribute) -> Option<Vec<Name>> {
        let context = self.context.as_ref()?;
        let mut path = self.path(&attribute.value, context)?;
        path.push(attribute.attr.id.clone());
        Some(path)
    }

    fn path(&self, expression: &Expr, context: &Name) -> Option<Vec<Name>> {
        match expression {
            Expr::Name(name) if name.id == *context => Some(Vec::new()),
            Expr::Name(name) => self
                .aliases
                .iter()
                .find(|(alias, _)| *alias == name.id)
                .map(|(_, path)| path.to_vec()),
            Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                if attr.id == "context"
                    && value.as_name_expr().is_some_and(|value| value.id == "bpy")
                {
                    return Some(Vec::new());
                }
                let mut path = self.path(value, context)?;
                path.push(attr.id.clone());
                Some(path)
            }
            _ => None,
        }
    }

    /// Whether the read of `path` at `range` is one the callback only tests, or one inside a
    /// region where its own test already narrows the path.
    fn skips(&self, range: TextRange, path: &[Name]) -> bool {
        self.tested.contains(&range)
            || self
                .regions
                .iter()
                .any(|(region_path, region)| **region_path == *path && region.contains_range(range))
    }
}

/// Whether the attribute read `attribute`, in the function scope `function`, is one the function
/// only tests for truth or `None`, or one its own test already narrows.
pub(super) fn is_tested_read<'db>(
    db: &'db dyn Db,
    function: ScopeId<'db>,
    attribute: &ast::ExprAttribute,
) -> bool {
    let paths = function_paths(db, function);
    paths
        .attribute_path(attribute)
        .is_some_and(|path| paths.skips(attribute.range(), &path))
}

/// The paths and tested reads of the function scope `function`, a method whose second parameter
/// is the context.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn function_paths<'db>(db: &'db dyn Db, function: ScopeId<'db>) -> FunctionPaths {
    let file: ProgramFile<'db> = function.program_file(db);
    let Some(node) = function.node(db).as_function() else {
        return FunctionPaths::default();
    };
    let module = parsed_module(db, file.python_file(db)).load(db);
    let node = node.node(&module);
    let parameters = &node.parameters;
    let Some(context) = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .nth(1)
        .map(|parameter| parameter.parameter.name.id.clone())
    else {
        return FunctionPaths::default();
    };
    let aliases = aliases(node, &context);
    let mut tests = Tests {
        context: &context,
        aliases: &aliases,
        tested: Vec::new(),
        regions: Vec::new(),
    };
    tests.block(&node.body);
    let Tests {
        tested, regions, ..
    } = tests;
    FunctionPaths {
        aliases: aliases
            .into_iter()
            .map(|(name, alias)| (name, alias.path.into_boxed_slice()))
            .collect(),
        context: Some(context),
        tested: tested.into_boxed_slice(),
        regions: regions.into_boxed_slice(),
    }
}

/// Finds the tests of a callback body for truth or `None`.
struct Tests<'a> {
    context: &'a Name,
    aliases: &'a FxHashMap<Name, Alias>,
    tested: Vec<TextRange>,
    regions: Vec<(Path, TextRange)>,
}

impl Tests<'_> {
    fn path(&self, expression: &Expr) -> Option<Path> {
        path_of(expression, self.context, self.aliases)
            .filter(|path| !path.is_empty())
            .map(Vec::into_boxed_slice)
    }

    fn block(&mut self, body: &[Stmt]) {
        let block_end = body.last().map(Ranged::end);
        for statement in body {
            match statement {
                Stmt::If(if_statement) => {
                    self.test(&if_statement.test);
                    if let (Some(first), Some(last)) =
                        (if_statement.body.first(), if_statement.body.last())
                    {
                        let range = TextRange::new(first.start(), last.end());
                        for path in self.truthy_paths(&if_statement.test) {
                            self.regions.push((path, range));
                        }
                    }
                    if if_statement.elif_else_clauses.is_empty()
                        && if_statement.body.last().is_some_and(exits)
                        && let Some(end) = block_end
                        && statement.end() < end
                    {
                        let range = TextRange::new(statement.end(), end);
                        for path in self.falsy_paths(&if_statement.test) {
                            self.regions.push((path, range));
                        }
                    }
                    self.block(&if_statement.body);
                    for clause in &if_statement.elif_else_clauses {
                        if let Some(test) = &clause.test {
                            self.test(test);
                        }
                        self.block(&clause.body);
                    }
                }
                Stmt::While(statement) => {
                    self.test(&statement.test);
                    self.block(&statement.body);
                    self.block(&statement.orelse);
                }
                Stmt::Assert(statement) => {
                    self.test(&statement.test);
                    if let Some(end) = block_end
                        && statement.end() < end
                    {
                        let range = TextRange::new(statement.end(), end);
                        for path in self.truthy_paths(&statement.test) {
                            self.regions.push((path, range));
                        }
                    }
                }
                Stmt::For(statement) => {
                    self.block(&statement.body);
                    self.block(&statement.orelse);
                }
                Stmt::With(statement) => self.block(&statement.body),
                Stmt::Try(statement) => {
                    self.block(&statement.body);
                    for handler in &statement.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        self.block(&handler.body);
                    }
                    self.block(&statement.orelse);
                    self.block(&statement.finalbody);
                }
                Stmt::Match(statement) => {
                    for case in &statement.cases {
                        self.block(&case.body);
                    }
                }
                _ => {}
            }
        }
    }

    /// Records the reads `test` tests for truth or `None`.
    fn test(&mut self, test: &Expr) {
        match test {
            Expr::BoolOp(ast::ExprBoolOp { values, .. }) => {
                for value in values {
                    self.test(value);
                }
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => self.test(operand),
            Expr::Compare(compare) => {
                if let Some((operand, op, other)) = single_comparison(compare)
                    && other.is_none_literal_expr()
                    && matches!(
                        op,
                        ast::CmpOp::Is | ast::CmpOp::IsNot | ast::CmpOp::Eq | ast::CmpOp::NotEq
                    )
                {
                    self.tested_read(operand);
                }
            }
            Expr::Call(call) => {
                if let Expr::Name(function) = &*call.func
                    && function.id == "bool"
                    && let [argument] = &*call.arguments.args
                {
                    self.tested_read(argument);
                }
            }
            Expr::Name(_) | Expr::Attribute(_) => self.tested_read(test),
            _ => {}
        }
    }

    fn tested_read(&mut self, expression: &Expr) {
        match expression {
            Expr::Attribute(_) => self.tested.push(expression.range()),
            Expr::Name(name) => {
                if let Some(alias) = self.aliases.get(&name.id) {
                    self.tested.push(alias.value);
                }
            }
            _ => {}
        }
    }

    /// The paths that are not `None` when `test` is true.
    fn truthy_paths(&self, test: &Expr) -> Vec<Path> {
        match test {
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::And,
                values,
                ..
            }) => values
                .iter()
                .flat_map(|value| self.truthy_paths(value))
                .collect(),
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => self.falsy_paths(operand),
            Expr::Compare(compare) => match single_comparison(compare) {
                Some((operand, ast::CmpOp::IsNot | ast::CmpOp::NotEq, other))
                    if other.is_none_literal_expr() =>
                {
                    self.path(operand).into_iter().collect()
                }
                _ => Vec::new(),
            },
            Expr::Call(call) => checked_argument(call)
                .and_then(|checked| self.path(checked))
                .into_iter()
                .collect(),
            Expr::Name(_) | Expr::Attribute(_) => self.path(test).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    /// The paths that are not `None` when `test` is false.
    fn falsy_paths(&self, test: &Expr) -> Vec<Path> {
        match test {
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::Or,
                values,
                ..
            }) => values
                .iter()
                .flat_map(|value| self.falsy_paths(value))
                .collect(),
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => self.truthy_paths(operand),
            Expr::Compare(compare) => match single_comparison(compare) {
                Some((operand, ast::CmpOp::Is | ast::CmpOp::Eq, other))
                    if other.is_none_literal_expr() =>
                {
                    self.path(operand).into_iter().collect()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

/// Whether `statement` leaves its block: `return`, `raise`, `continue` or `break`.
fn exits(statement: &Stmt) -> bool {
    matches!(
        statement,
        Stmt::Return(_) | Stmt::Raise(_) | Stmt::Continue(_) | Stmt::Break(_)
    )
}
