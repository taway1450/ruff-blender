//! Reads that Blender's own bookkeeping narrows, wherever they are, without a `poll()`:
//!
//! - the `gpencil_settings` of a Grease Pencil paint mode's brush, never `None` once the brush is
//!   read ([`is_set_once_read`]);
//! - `area.spaces.active` after `area.type == 'VIEW_3D'`: an area's first space is the space of
//!   its type;
//! - `context.region` inside `with context.temp_override(region=region):`, the value passed.
//!
//! ```python
//! for area in screen.areas:
//!     if area.type == 'VIEW_3D':
//!         area.spaces.active.shading      # `SpaceView3D`
//! with context.temp_override(area=area):
//!     context.area.tag_redraw()           # `Area`, the value passed
//! ```
//!
//! These are the code's own tests and Blender's own invariants, so none of them is reported. The
//! Pointer aliases between poll facts (`node_tree` and `edit_tree`) are applied by
//! [`super::poll_checks`].

use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::comparable::ComparableExpr;
use ruff_python_ast::visitor::source_order::{
    SourceOrderVisitor, TraversalSignal, walk_expr, walk_stmt,
};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_blender::is_set_once_read;
use ty_python_core::{ProgramFile, semantic_index};

use crate::Db;
use crate::place::PlaceAndQualifiers;
use crate::types::context::InferContext;
use crate::types::infer::{TypeContext, infer_expression_types};
use crate::types::{Type, UnionType};

use super::context_members::{enclosing_function, is_context_instance, space_subclass};
use super::member::without_none;
use super::poll_checks::{exits, function_paths, single_comparison, string_literals};
use super::symbols::bpy_types_class_name;

/// The type of the attribute read `attribute` on `receiver`, where ty found `place`, when Blender's
/// bookkeeping or the code's own `area.type` test or `temp_override` narrows it.
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
    let original = place.place.ignore_possibly_undefined()?;
    let name = attribute.attr.id.as_str();
    let narrowed = if name == "gpencil_settings" {
        brush_settings(db, file, attribute, original)
    } else if name == "active" && is_bpy_types_instance(db, env, receiver, "AreaSpaces") {
        active_space(db, env, file, attribute)
    } else if is_context_instance(db, env, receiver) && *has_temp_override(db, file) {
        overridden(db, env, file, attribute, original)
    } else {
        None
    }?;
    (narrowed != original).then(|| place.map_type(|_| narrowed))
}

/// `<gp paint>.brush.gpencil_settings`, a path from the context: `original` without `None`.
fn brush_settings<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
    original: Type<'db>,
) -> Option<Type<'db>> {
    let function = enclosing_function(db, file, attribute)?;
    let path = function_paths(db, function).attribute_path(attribute)?;
    is_set_once_read(&path).then(|| without_none(db, original))
}

/// `X.spaces.active` where a test of `X.type` holds: the union of the spaces of the area types it
/// allows.
fn active_space<'db>(
    db: &'db dyn Db,
    env: &crate::ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
) -> Option<Type<'db>> {
    let Expr::Attribute(spaces) = &*attribute.value else {
        return None;
    };
    let area = ComparableExpr::from(&*spaces.value);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let mut holding = Holding::new(attribute.range());
    holding.visit_body(module.suite());
    let values = holding
        .tests
        .iter()
        .rev()
        .find_map(|(test, positive)| area_types(test, *positive, &area))?;
    let spaces = values
        .iter()
        .map(|value| space_subclass(db, env, value))
        .collect::<Option<Vec<_>>>()?;
    Some(UnionType::from_elements(db, env, spaces))
}

/// The area types `test` allows for the area `area` when it is `positive` (true) or not:
/// `area.type == 'X'`, `in {...}`, `!=` / `not in` when false, also among the operands of `and`
/// (true) or `or` (false).
fn area_types(test: &Expr, positive: bool, area: &ComparableExpr) -> Option<Vec<ast::name::Name>> {
    match test {
        Expr::UnaryOp(ast::ExprUnaryOp {
            op: ast::UnaryOp::Not,
            operand,
            ..
        }) => area_types(operand, !positive, area),
        Expr::BoolOp(ast::ExprBoolOp { op, values, .. })
            if (*op == ast::BoolOp::And) == positive =>
        {
            values
                .iter()
                .rev()
                .find_map(|value| area_types(value, positive, area))
        }
        Expr::Compare(compare) => {
            let (operand, op, other) = single_comparison(compare)?;
            let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = operand else {
                return None;
            };
            let allows = match op {
                ast::CmpOp::Eq | ast::CmpOp::In => positive,
                ast::CmpOp::NotEq | ast::CmpOp::NotIn => !positive,
                _ => return None,
            };
            (allows && attr.id == "type" && ComparableExpr::from(&**value) == *area)
                .then(|| string_literals(other))
                .flatten()
        }
        _ => None,
    }
}

/// Whether `file` calls `temp_override` anywhere, so that a context read needs a look for it.
#[salsa::tracked(returns(ref))]
fn has_temp_override<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> bool {
    source_text(db, file.file(db)).contains("temp_override")
}

/// `C.k` inside `with C.temp_override(k=v):`, the innermost such block: `v`'s type, when it is
/// known and fits the member.
fn overridden<'db>(
    db: &'db dyn Db,
    env: &crate::ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
    original: Type<'db>,
) -> Option<Type<'db>> {
    let receiver = ComparableExpr::from(&*attribute.value);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let mut holding = Holding::new(attribute.range());
    holding.visit_body(module.suite());
    let (call, value) = holding.overrides.iter().rev().find_map(|call| {
        let call: &ast::ExprCall = call;
        let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = &*call.func else {
            return None;
        };
        if attr.id != "temp_override" || ComparableExpr::from(&**value) != receiver {
            return None;
        }
        call.arguments
            .keywords
            .iter()
            .find(|keyword| {
                keyword
                    .arg
                    .as_ref()
                    .is_some_and(|arg| arg.id == attribute.attr.id)
            })
            .map(|keyword| (call, &keyword.value))
    })?;
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

/// Whether `ty` is an instance of the `bpy.types` class `name`.
fn is_bpy_types_instance<'db>(
    db: &'db dyn Db,
    env: &crate::ProgramEnvironment<'db>,
    ty: Type<'db>,
    name: &str,
) -> bool {
    let Type::NominalInstance(instance) = ty else {
        return false;
    };
    instance
        .class(db, env)
        .class_literal(db)
        .as_static()
        .is_some_and(|class| bpy_types_class_name(db, class) == Some(name))
}

/// The tests that hold where the expression at `range` is evaluated, in its function (or module),
/// outermost first: the test of an `if` or `while` around it (false in an `elif` or `else`), of
/// an earlier `if T: <return / continue / ...>` in the same block (false), the operands before it
/// in `and` (true) or `or` (false), the test of a conditional expression and the conditions of a
/// comprehension. Also the `temp_override` calls of the `with` blocks around it. A nested `def`,
/// `class` or `lambda` starts afresh: it may run later.
struct Holding<'a> {
    range: TextRange,
    tests: Vec<(&'a Expr, bool)>,
    overrides: Vec<&'a ast::ExprCall>,
}

impl<'a> Holding<'a> {
    fn new(range: TextRange) -> Self {
        Self {
            range,
            tests: Vec::new(),
            overrides: Vec::new(),
        }
    }

    fn contains(&self, node: impl Ranged) -> bool {
        node.range().contains_range(self.range)
    }

    fn contained_in(&self, body: &[Stmt]) -> bool {
        body.iter().any(|statement| self.contains(statement))
    }

    fn clear(&mut self) {
        self.tests.clear();
        self.overrides.clear();
    }
}

impl<'a> SourceOrderVisitor<'a> for Holding<'a> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if self.contains(node) {
            TraversalSignal::Traverse
        } else {
            TraversalSignal::Skip
        }
    }

    fn visit_body(&mut self, body: &'a [Stmt]) {
        for statement in body {
            if self.contains(statement) {
                self.visit_stmt(statement);
                return;
            }
            if let Stmt::If(ast::StmtIf {
                test,
                body,
                elif_else_clauses,
                ..
            }) = statement
                && elif_else_clauses.is_empty()
                && body.last().is_some_and(exits)
            {
                self.tests.push((test, false));
            }
        }
    }

    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if !self.contains(statement) {
            return;
        }
        match statement {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {
                self.clear();
                walk_stmt(self, statement);
            }
            Stmt::If(ast::StmtIf {
                test,
                body,
                elif_else_clauses,
                ..
            }) => {
                if self.contains(&**test) {
                    self.visit_expr(test);
                    return;
                }
                if self.contained_in(body) {
                    self.tests.push((test, true));
                    self.visit_body(body);
                    return;
                }
                self.tests.push((test, false));
                for clause in elif_else_clauses {
                    if let Some(test) = &clause.test {
                        if self.contains(test) {
                            self.visit_expr(test);
                            return;
                        }
                        if self.contained_in(&clause.body) {
                            self.tests.push((test, true));
                            self.visit_body(&clause.body);
                            return;
                        }
                        self.tests.push((test, false));
                    } else if self.contained_in(&clause.body) {
                        self.visit_body(&clause.body);
                        return;
                    }
                }
            }
            Stmt::While(ast::StmtWhile { test, body, .. }) => {
                if self.contained_in(body) {
                    self.tests.push((test, true));
                }
                walk_stmt(self, statement);
            }
            Stmt::With(ast::StmtWith { items, body, .. }) => {
                if self.contained_in(body) {
                    self.overrides.extend(
                        items
                            .iter()
                            .filter_map(|item| item.context_expr.as_call_expr()),
                    );
                }
                walk_stmt(self, statement);
            }
            _ => walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        if !self.contains(expression) {
            return;
        }
        match expression {
            Expr::Lambda(_) => {
                self.clear();
                walk_expr(self, expression);
            }
            Expr::BoolOp(ast::ExprBoolOp { op, values, .. }) => {
                for value in values {
                    if self.contains(value) {
                        self.visit_expr(value);
                        return;
                    }
                    self.tests.push((value, *op == ast::BoolOp::And));
                }
            }
            Expr::If(ast::ExprIf {
                test, body, orelse, ..
            }) => {
                if self.contains(&**test) {
                    self.visit_expr(test);
                } else {
                    self.tests.push((test, self.contains(&**body)));
                    self.visit_expr(if self.contains(&**body) { body } else { orelse });
                }
            }
            Expr::ListComp(ast::ExprListComp {
                elt, generators, ..
            })
            | Expr::SetComp(ast::ExprSetComp {
                elt, generators, ..
            })
            | Expr::Generator(ast::ExprGenerator {
                elt, generators, ..
            }) => self.comprehension(generators, &[elt]),
            Expr::DictComp(ast::ExprDictComp {
                key,
                value,
                generators,
                ..
            }) => {
                let elements: Vec<&Expr> = key.as_deref().into_iter().chain([&**value]).collect();
                self.comprehension(generators, &elements);
            }
            _ => walk_expr(self, expression),
        }
    }
}

impl<'a> Holding<'a> {
    /// A comprehension: the conditions before the expression at `range` hold, and all of them for
    /// its element.
    fn comprehension(&mut self, generators: &'a [ast::Comprehension], elements: &[&'a Expr]) {
        for generator in generators {
            if self.contains(&generator.iter) {
                self.visit_expr(&generator.iter);
                return;
            }
            for condition in &generator.ifs {
                if self.contains(condition) {
                    self.visit_expr(condition);
                    return;
                }
                self.tests.push((condition, true));
            }
        }
        for element in elements {
            if self.contains(*element) {
                self.visit_expr(element);
                return;
            }
        }
    }
}
