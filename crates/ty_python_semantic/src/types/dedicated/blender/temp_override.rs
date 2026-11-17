//! `context.k` inside `with context.temp_override(k=v):`: the value passed.
//!
//! `Context.temp_override(**members)` puts the members passed in front of the context's own until
//! the `with` block ends (`bpy_rna_context.cc`), so a read of one of them in the block is `v`.
//!
//! ```python
//! with context.temp_override(area=area):
//!     context.area.tag_redraw()           # `Area`, the value passed
//! ```
//!
//! The code's own control flow, so it is not reported. The blocks of a scope are read once, by
//! [`temp_overrides`]. A block's region is its body up to the first statement that rebinds the
//! name its receiver starts with (`context = other`), or up to the loop in the body around that
//! statement, which may run it before a read written above it.

use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{
    SourceOrderVisitor, TraversalSignal, walk_except_handler, walk_expr, walk_pattern, walk_stmt,
};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_core::scope::{NodeWithScopeKind, ScopeId};
use ty_python_core::{ProgramFile, semantic_index};

use crate::place::PlaceAndQualifiers;
use crate::types::Type;
use crate::types::context::InferContext;
use crate::types::infer::{TypeContext, infer_expression_types};
use crate::{Db, ProgramEnvironment};

use super::context_members::is_context_instance;

/// The type of the context member read `attribute` on `receiver`, where ty found `place`, when a
/// `temp_override` block around it passed that member.
pub(super) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
    place: PlaceAndQualifiers<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    if !attribute.ctx.is_load() {
        return None;
    }
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    if !*mentions_temp_override(db, file) || !is_context_instance(db, env, receiver) {
        return None;
    }
    let original = place.place.ignore_possibly_undefined()?;
    let narrowed = overridden(db, env, file, attribute, original)?;
    (narrowed != original).then(|| place.map_type(|_| narrowed))
}

/// Whether `file` mentions `temp_override` at all, so that a context read needs a look for it.
#[salsa::tracked(returns(ref))]
fn mentions_temp_override<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> bool {
    source_text(db, file.file(db)).contains("temp_override")
}

/// `C.k` in the region of the innermost `with C.temp_override(k=v):` of its scope: `v`'s type, when
/// it is known and fits the member.
fn overridden<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
    original: Type<'db>,
) -> Option<Type<'db>> {
    let receiver = dotted(&attribute.value)?;
    let index = semantic_index(db, file);
    let scope = index.try_expression_scope_id(&*attribute.value)?;
    // A list, set or dict comprehension runs where it is written; a generator, lambda or nested
    // `def` may run after the block.
    let (scope, _) = index.ancestor_scopes(scope).find(|(_, scope)| {
        !matches!(
            scope.node(),
            NodeWithScopeKind::ListComprehension(_)
                | NodeWithScopeKind::SetComprehension(_)
                | NodeWithScopeKind::DictComprehension(_)
        )
    })?;
    let block = temp_overrides(db, scope.to_scope_id(db, file))
        .iter()
        .rev()
        .find(|block| {
            block.region.contains_range(attribute.range())
                && *block.receiver == *receiver
                && block.members.contains(&attribute.attr.id)
        })?;
    let module = parsed_module(db, file.python_file(db)).load(db);
    let mut finder = CallFinder {
        range: block.call,
        call: None,
    };
    finder.visit_body(module.suite());
    let call = finder.call?;
    let value = &call
        .arguments
        .keywords
        .iter()
        .find(|keyword| {
            keyword
                .arg
                .as_ref()
                .is_some_and(|arg| arg.id == attribute.attr.id)
        })?
        .value;
    // The call is a standalone expression (ty infers every call on its own, for its return), so
    // its argument's type is known without inferring the `with` statement again.
    let call = semantic_index(db, file).try_expression(call)?;
    let ty = infer_expression_types(db, call, TypeContext::default()).expression_type(value);
    (!ty.is_unknown()
        && !ty.is_dynamic()
        && !ty.is_never()
        && ty.is_assignable_to(db, env, original))
    .then_some(ty)
}

/// A `with C.temp_override(k=v, ...):` block of a scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
struct TempOverride {
    /// Where the members passed are read back: the body, up to a rebinding of `C`.
    region: TextRange,
    /// `C` as a dotted name: `context`, `bpy.context`.
    receiver: Box<[Name]>,
    /// The members passed by keyword.
    members: Box<[Name]>,
    /// The `temp_override(...)` call.
    call: TextRange,
}

/// The `temp_override` blocks of the scope `scope`, a function or the module, in source order (an
/// outer block before the blocks in it). Other scopes have none of their own.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn temp_overrides<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> Box<[TempOverride]> {
    let file = scope.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let body = match scope.node(db) {
        NodeWithScopeKind::Function(function) => &function.node(&module).body[..],
        NodeWithScopeKind::Module => &module.suite()[..],
        _ => return Box::default(),
    };
    let mut collector = Collector::default();
    collector.visit_body(body);
    collector.blocks.into_boxed_slice()
}

#[derive(Default)]
struct Collector {
    blocks: Vec<TempOverride>,
}

impl<'a> SourceOrderVisitor<'a> for Collector {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            // Another scope.
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::With(with) => {
                for item in &with.items {
                    if let Some(block) = temp_override(with, item) {
                        self.blocks.push(block);
                    }
                }
                walk_stmt(self, statement);
            }
            _ => walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, _expression: &'a Expr) {}
}

/// The block of `with` for its item `item`, when that is a `C.temp_override(...)` call.
fn temp_override(with: &ast::StmtWith, item: &ast::WithItem) -> Option<TempOverride> {
    let call = item.context_expr.as_call_expr()?;
    let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = &*call.func else {
        return None;
    };
    if attr.id != "temp_override" {
        return None;
    }
    let receiver = dotted(value)?;
    let root = receiver.first()?;
    let mut rebinding = Rebinding {
        name: root,
        loop_start: None,
        at: None,
    };
    for item in &with.items {
        if let Some(target) = &item.optional_vars {
            rebinding.visit_expr(target);
        }
    }
    if rebinding.at.is_some() {
        return None;
    }
    rebinding.visit_body(&with.body);
    let start = with.body.first()?.start();
    let end = rebinding
        .at
        .unwrap_or_else(|| with.body.last().map_or(start, Ranged::end));
    let members = call
        .arguments
        .keywords
        .iter()
        .filter_map(|keyword| keyword.arg.as_ref().map(|arg| arg.id.clone()))
        .collect();
    Some(TempOverride {
        region: TextRange::new(start, end.max(start)),
        receiver,
        members,
        call: call.range(),
    })
}

/// `a.b.c` as its names.
fn dotted(expression: &Expr) -> Option<Box<[Name]>> {
    fn push(expression: &Expr, names: &mut Vec<Name>) -> Option<()> {
        match expression {
            Expr::Name(name) => names.push(name.id.clone()),
            Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                push(value, names)?;
                names.push(attr.id.clone());
            }
            _ => return None,
        }
        Some(())
    }
    let mut names = Vec::new();
    push(expression, &mut names)?;
    Some(names.into_boxed_slice())
}

/// Where a body first rebinds `name`: the start of the binding statement, or of the outermost loop
/// in the body around it. Any binding counts (an assignment, a `for` or `with` target, `del`, an
/// import, a `def`, an `except` or `match` capture), also in a nested scope, which may declare it
/// `nonlocal`.
struct Rebinding<'n> {
    name: &'n Name,
    loop_start: Option<TextSize>,
    at: Option<TextSize>,
}

impl Rebinding<'_> {
    fn bound(&mut self, name: &str, at: TextSize) {
        if name == self.name.as_str() {
            let at = self.loop_start.unwrap_or(at);
            self.at = Some(self.at.map_or(at, |first| first.min(at)));
        }
    }
}

impl<'a> SourceOrderVisitor<'a> for Rebinding<'_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::FunctionDef(ast::StmtFunctionDef { name, .. })
            | Stmt::ClassDef(ast::StmtClassDef { name, .. }) => {
                self.bound(name, statement.start());
            }
            _ => {}
        }
        if matches!(statement, Stmt::For(_) | Stmt::While(_)) && self.loop_start.is_none() {
            self.loop_start = Some(statement.start());
            walk_stmt(self, statement);
            self.loop_start = None;
        } else {
            walk_stmt(self, statement);
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        if let Expr::Name(name) = expression
            && !name.ctx.is_load()
        {
            self.bound(&name.id, name.start());
        }
        walk_expr(self, expression);
    }

    fn visit_alias(&mut self, alias: &'a ast::Alias) {
        let bound = alias.asname.as_ref().unwrap_or(&alias.name);
        let first = bound.id.split('.').next().unwrap_or_default();
        self.bound(first, alias.start());
    }

    fn visit_except_handler(&mut self, handler: &'a ast::ExceptHandler) {
        let ast::ExceptHandler::ExceptHandler(inner) = handler;
        if let Some(name) = &inner.name {
            self.bound(name, handler.start());
        }
        walk_except_handler(self, handler);
    }

    fn visit_pattern(&mut self, pattern: &'a ast::Pattern) {
        let name = match pattern {
            ast::Pattern::MatchAs(ast::PatternMatchAs { name, .. })
            | ast::Pattern::MatchStar(ast::PatternMatchStar { name, .. }) => name.as_ref(),
            ast::Pattern::MatchMapping(ast::PatternMatchMapping { rest, .. }) => rest.as_ref(),
            _ => None,
        };
        if let Some(name) = name {
            self.bound(name, pattern.start());
        }
        walk_pattern(self, pattern);
    }
}

/// The call whose range is `range`, found by descending only into the nodes around it.
struct CallFinder<'a> {
    range: TextRange,
    call: Option<&'a ast::ExprCall>,
}

impl<'a> SourceOrderVisitor<'a> for CallFinder<'a> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if self.call.is_none() && node.range().contains_range(self.range) {
            TraversalSignal::Traverse
        } else {
            TraversalSignal::Skip
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        if let Expr::Call(call) = expression
            && call.range() == self.range
        {
            self.call = Some(call);
            return;
        }
        walk_expr(self, expression);
    }
}
