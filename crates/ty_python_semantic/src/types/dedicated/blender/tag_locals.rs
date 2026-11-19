//! A Type tag tested through a local copy of it: Blender's UI code reads the tag once and tests
//! the local in an `if` / `elif` chain, so a member read of the struct in a branch comes from the
//! subclass the branch's test names.
//!
//! ```python
//! strip = context.active_strip
//! strip_type = strip.type
//! if strip_type == 'SPEED':
//!     strip.speed_control       # read from `SpeedControlStrip`
//! elif strip_type in {'CROSS', 'WIPE'}:
//!     strip.input_1             # read from `CrossStrip | WipeStrip`
//! ```
//!
//! ty narrows only the places a test names, and the test names the local, not the struct; so the
//! narrowing is applied to the member reads of the struct instead, by syntax alone, within one
//! function or module-level script. A read narrows when it is in the body of an `if`, `elif` or
//! `else` clause whose own test, or an earlier clause's failed test, is `local == 'LIT'`,
//! `local != 'LIT'`, `local in {...}` or `local not in {...}` (a literal or a tuple, list or set
//! display of them), where:
//!
//! - the last binding of `local` before the `if` is `local = struct.tag`, in a block that holds the
//!   `if` (so it always ran before the test);
//! - `struct` is not bound again between that assignment and the read, nor after the read in a
//!   loop around it that the assignment is outside of;
//! - the scope writes no tag of `struct` (`area.type = ...`), as for a direct test.
//!
//! The struct itself keeps its type: `row.prop(strip, "speed_control")` and a hover on `strip` see
//! the base. A read of the tag itself is left to ty.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_body, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_blender::tag_writers;
use ty_python_core::global_scope;
use ty_python_core::scope::ScopeId;

use crate::Db;
use crate::place::PlaceAndQualifiers;
use crate::types::context::InferContext;
use crate::types::{Type, UnionType};

use super::context_members::enclosing_function;
use super::poll_checks::{single_comparison, string_literals};
use super::type_tags::narrow_by_values;

/// The member `attribute` of `receiver` read from the subclass that the tests of a local copy of
/// its Type tag name. `None` leaves the read as it is.
pub(super) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    if !attribute.ctx.is_load() {
        return None;
    }
    let Expr::Name(name) = &*attribute.value else {
        return None;
    };
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let scope = enclosing_function(db, file, attribute).unwrap_or_else(|| global_scope(db, file));
    let locals = tag_locals(db, scope);
    let read = attribute.range();
    let mut narrowed = receiver;
    for test in locals
        .tests
        .iter()
        .filter(|test| test.body.contains_range(read))
    {
        let Some(copy) = locals.copy_before(&test.local, test.statement.start()) else {
            continue;
        };
        if copy.receiver != name.id
            || attribute.attr.id == copy.tag
            || copy.block_end < test.statement.end()
            || locals.rebinds(&name.id, copy.statement, read)
            || locals.writes_tag(&name.id, &copy.tag)
        {
            continue;
        }
        narrowed = narrow(db, env, narrowed, &copy.tag, &test.values, test.is_one_of)?;
    }
    if narrowed == receiver {
        return None;
    }
    let member = narrowed.member(db, env, attribute.attr.id.as_str());
    member.place.ignore_possibly_undefined()?;
    Some(member)
}

/// `ty` once its Type tag `tag` is known to be one of `values` (`is_one_of`) or none of them. A
/// union member whose tag is a literal (`SpeedControlStrip.type: Literal['SPEED']`) is kept or
/// dropped by it; any other member is narrowed by the Type tag table, and kept as it is when the
/// table cannot narrow it. `None` when nothing is left.
fn narrow<'db>(
    db: &'db dyn Db,
    env: &crate::ProgramEnvironment<'db>,
    ty: Type<'db>,
    tag: &Name,
    values: &[Name],
    is_one_of: bool,
) -> Option<Type<'db>> {
    let elements = match ty {
        Type::Union(union) => union.elements(db).to_vec(),
        _ => vec![ty],
    };
    let narrowed: Vec<Type<'db>> = elements
        .into_iter()
        .filter_map(|element| {
            let literal = element
                .member(db, env, tag.as_str())
                .place
                .ignore_possibly_undefined()
                .and_then(|member| member.as_literal_value()?.as_string())
                .map(|value| Name::new(value.value(db)));
            match literal {
                Some(value) => (values.contains(&value) == is_one_of).then_some(element),
                None => Some(
                    narrow_by_values(db, env, element, tag.as_str(), values, is_one_of)
                        .unwrap_or(element),
                ),
            }
        })
        .collect();
    (!narrowed.is_empty()).then(|| UnionType::from_elements(db, env, narrowed))
}

/// `local = receiver.tag`.
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
struct TagCopy {
    statement: TextRange,
    /// The end of the innermost block holding the assignment.
    block_end: TextSize,
    local: Name,
    receiver: Name,
    tag: Name,
}

/// A clause of an `if` chain in which the local is known to be one of `values`, or none of them.
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
struct LocalTest {
    /// The whole `if` statement: the copy must come before it.
    statement: TextRange,
    /// The clause's body.
    body: TextRange,
    local: Name,
    values: Box<[Name]>,
    is_one_of: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, get_size2::GetSize)]
struct TagLocals {
    copies: Box<[TagCopy]>,
    tests: Box<[LocalTest]>,
    /// The names bound in the scope, with where.
    bindings: Box<[(Name, TextRange)]>,
    /// The attributes written on a name (`area.type = ...`).
    writes: Box<[(Name, Name)]>,
    /// The bodies of the `for` and `while` loops.
    loops: Box<[TextRange]>,
}

impl TagLocals {
    /// The copy of a tag that the last binding of `local` before `offset` makes, if it is one.
    fn copy_before(&self, local: &Name, offset: TextSize) -> Option<&TagCopy> {
        let (_, binding) = self
            .bindings
            .iter()
            .filter(|(name, range)| name == local && range.end() <= offset)
            .max_by_key(|(_, range)| range.start())?;
        self.copies
            .iter()
            .find(|copy| copy.statement.contains_range(*binding))
    }

    /// Whether `name` is bound again after the copy `copy` and before the read `read`, or after the
    /// read in a loop around it that does not hold the copy.
    fn rebinds(&self, name: &Name, copy: TextRange, read: TextRange) -> bool {
        self.bindings.iter().any(|(bound, range)| {
            bound == name
                && ((range.start() >= copy.end() && range.end() <= read.start())
                    || (range.start() > read.end()
                        && self.loops.iter().any(|body| {
                            body.contains_range(read)
                                && body.contains_range(*range)
                                && !body.contains_range(copy)
                        })))
        })
    }

    /// Whether the scope writes the Type tag `tag` of `name`, or an attribute that sets it.
    fn writes_tag(&self, name: &Name, tag: &Name) -> bool {
        self.writes.iter().any(|(receiver, attribute)| {
            receiver == name && tag_writers(tag).any(|writer| writer == attribute.as_str())
        })
    }
}

/// The tag copies, local tests and bindings directly in `scope`, a function or a module; not
/// those in a nested function, class or lambda.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn tag_locals<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> TagLocals {
    let file = scope.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let body = match scope.node(db).as_function() {
        Some(function) => &function.node(&module).body[..],
        None if scope.file_scope_id(db).is_global() => &module.suite()[..],
        None => return TagLocals::default(),
    };
    let mut collector = Collector::default();
    collector.visit_body(body);
    TagLocals {
        copies: collector.copies.into_boxed_slice(),
        tests: collector.tests.into_boxed_slice(),
        bindings: collector.bindings.into_boxed_slice(),
        writes: collector.writes.into_boxed_slice(),
        loops: collector.loops.into_boxed_slice(),
    }
}

#[derive(Default)]
struct Collector {
    copies: Vec<TagCopy>,
    tests: Vec<LocalTest>,
    bindings: Vec<(Name, TextRange)>,
    writes: Vec<(Name, Name)>,
    loops: Vec<TextRange>,
    /// The ends of the blocks being visited, innermost last.
    block_ends: Vec<TextSize>,
}

/// The local, values and whether the test holds when the local is one of them, of
/// `local == 'LIT'`, `local != 'LIT'`, `local in {...}` or `local not in {...}`.
fn local_test(test: &Expr) -> Option<(&Name, Vec<Name>, bool)> {
    let Expr::Compare(compare) = test else {
        return None;
    };
    let (Expr::Name(local), operator, values) = single_comparison(compare)? else {
        return None;
    };
    let is_one_of = match operator {
        ast::CmpOp::Eq | ast::CmpOp::In => true,
        ast::CmpOp::NotEq | ast::CmpOp::NotIn => false,
        _ => return None,
    };
    Some((&local.id, string_literals(values)?, is_one_of))
}

fn body_range(body: &[Stmt]) -> Option<TextRange> {
    Some(TextRange::new(body.first()?.start(), body.last()?.end()))
}

impl<'a> Visitor<'a> for Collector {
    fn visit_body(&mut self, body: &'a [Stmt]) {
        if let Some(last) = body.last() {
            self.block_ends.push(last.end());
            walk_body(self, body);
            self.block_ends.pop();
        }
    }

    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::FunctionDef(function) => {
                self.bindings
                    .push((function.name.id.clone(), function.name.range()));
            }
            Stmt::ClassDef(class) => {
                self.bindings
                    .push((class.name.id.clone(), class.name.range()));
            }
            Stmt::Assign(ast::StmtAssign { targets, value, .. })
                if let [Expr::Name(local)] = &targets[..]
                    && let Expr::Attribute(read) = &**value
                    && let Expr::Name(receiver) = &*read.value =>
            {
                self.copies.push(TagCopy {
                    statement: statement.range(),
                    block_end: self.block_ends.last().copied().unwrap_or(statement.end()),
                    local: local.id.clone(),
                    receiver: receiver.id.clone(),
                    tag: read.attr.id.clone(),
                });
                walk_stmt(self, statement);
            }
            Stmt::If(if_statement) => {
                // What the earlier clauses' failed tests say, for the later clauses.
                let mut failed: Vec<(Name, Vec<Name>, bool)> = Vec::new();
                let clauses = std::iter::once((Some(&*if_statement.test), &if_statement.body[..]))
                    .chain(
                        if_statement
                            .elif_else_clauses
                            .iter()
                            .map(|clause| (clause.test.as_ref(), &clause.body[..])),
                    );
                for (test, body) in clauses {
                    let Some(range) = body_range(body) else {
                        continue;
                    };
                    for (local, values, is_one_of) in &failed {
                        self.tests.push(LocalTest {
                            statement: statement.range(),
                            body: range,
                            local: local.clone(),
                            values: values.clone().into_boxed_slice(),
                            is_one_of: !is_one_of,
                        });
                    }
                    if let Some((local, values, is_one_of)) = test.and_then(local_test) {
                        self.tests.push(LocalTest {
                            statement: statement.range(),
                            body: range,
                            local: local.clone(),
                            values: values.clone().into_boxed_slice(),
                            is_one_of,
                        });
                        failed.push((local.clone(), values, is_one_of));
                    }
                }
                walk_stmt(self, statement);
            }
            Stmt::For(ast::StmtFor { body, .. }) | Stmt::While(ast::StmtWhile { body, .. }) => {
                if let Some(range) = body_range(body) {
                    self.loops.push(range);
                }
                walk_stmt(self, statement);
            }
            _ => walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Lambda(_) => {}
            Expr::Name(name) if !name.ctx.is_load() => {
                self.bindings.push((name.id.clone(), name.range()));
            }
            Expr::Attribute(target)
                if !target.ctx.is_load()
                    && let Expr::Name(name) = &*target.value =>
            {
                self.writes.push((name.id.clone(), target.attr.id.clone()));
            }
            _ => walk_expr(self, expression),
        }
    }
}
