//! A test case's assertion statement narrows the code after it, as an `assert` does:
//!
//! ```python
//! ob = bpy.data.objects.get("Cube")
//! self.assertIsNotNone(ob)  # as `assert ob is not None`
//! ob.name
//! ```
//!
//! The semantic index records the call as a narrowing predicate (`ty_python_core`'s `builder`), and
//! `narrow.rs` reads it here. This file is a child module of `narrow.rs` (declared there with
//! `#[path]`), so it can use the narrowing builder's private helpers.

use ruff_python_ast as ast;
use ty_blender::UnittestAssertion;

use super::{
    Expression, NarrowingConstraint, NarrowingConstraints, NarrowingConstraintsBuilder, PlaceExpr, Type,
};

impl<'db> NarrowingConstraintsBuilder<'db, '_> {
    /// The constraints of `call` when it is a test case's assertion (`self.assertIsNotNone(x)`), or
    /// `None` when it isn't one. Only the positive branch narrows: the code after a failed assertion
    /// never runs.
    pub(super) fn evaluate_unittest_assertion(
        &mut self,
        call: &ast::ExprCall,
        expression: Expression<'db>,
        is_positive: bool,
    ) -> Option<Option<NarrowingConstraints<'db>>> {
        if !is_positive {
            return None;
        }
        Some(match ty_blender::unittest_assertion(call)? {
            UnittestAssertion::IsNotNone(target) => {
                let place = PlaceExpr::try_from_expr(target).map(|place| self.expect_place(&place));
                place.map(|place| {
                    let not_none = Type::none(self.db, &self.env).negate(self.db, &self.env);
                    NarrowingConstraints::from_iter([(place, NarrowingConstraint::intersection(not_none))])
                })
            }
            UnittestAssertion::True(target) => self.evaluate_expression_node_predicate(target, expression, true),
        })
    }
}
