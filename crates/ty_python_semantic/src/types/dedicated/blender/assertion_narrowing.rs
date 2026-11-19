//! A test case's assertion statement narrows the code after it, as an `assert` does:
//!
//! ```python
//! ob = bpy.data.objects.get("Cube")
//! self.assertIsNotNone(ob)  # as `assert ob is not None`
//! ob.name
//! ```
//!
//! `assertTrue` / `assertFalse` narrow as `assert x` / `assert not x`, `assertIsNone` as
//! `assert x is None`, and `assertIsInstance(x, classinfo)` as `assert isinstance(x, classinfo)`.
//!
//! Only a `unittest.TestCase` narrows so: its assertion raises when it fails. A mixin's method
//! that calls `self.assertIsNotNone` does not know `self` is a test case, and another class's
//! method of that name may return.
//!
//! The semantic index records the call as a narrowing predicate (`ty_python_core`'s `builder`), and
//! `narrow.rs` reads it here. This file is a child module of `narrow.rs` (declared there with
//! `#[path]`), so it can use the narrowing builder's private helpers.

use ruff_python_ast as ast;
use ty_blender::UnittestAssertion;

use super::{
    ClassInfoConstraintFunction, Expression, NarrowingConstraint, NarrowingConstraints,
    NarrowingConstraintsBuilder, PlaceExpr, Type,
};
use crate::types::blender::is_test_case_instance;
use crate::types::infer::ExpressionInference;

impl<'db> NarrowingConstraintsBuilder<'db, '_> {
    /// The constraints of `call` when it is a test case's assertion (`self.assertIsNotNone(x)`), or
    /// `None` when it isn't one. Only the positive branch narrows: the code after a failed assertion
    /// never runs.
    pub(super) fn evaluate_unittest_assertion(
        &mut self,
        call: &ast::ExprCall,
        expression: Expression<'db>,
        inference: &ExpressionInference<'db>,
        is_positive: bool,
    ) -> Option<Option<NarrowingConstraints<'db>>> {
        if !is_positive {
            return None;
        }
        let assertion = ty_blender::unittest_assertion(call)?;
        // `self` must be a `TestCase`: another class's `assertIsNotNone` may return when the
        // value is None.
        let receiver = call.func.as_attribute_expr()?.value.as_ref();
        if !is_test_case_instance(self.db, &self.env, inference.expression_type(receiver)) {
            return None;
        }
        Some(match assertion {
            UnittestAssertion::IsNotNone(target) => {
                let not_none = Type::none(self.db, &self.env).negate(self.db, &self.env);
                self.place_intersection(target, not_none)
            }
            UnittestAssertion::IsNone(target) => {
                self.place_intersection(target, Type::none(self.db, &self.env))
            }
            UnittestAssertion::IsInstance(target, classinfo) => {
                // As `isinstance(x, classinfo)` narrows in an `assert`; a `classinfo` that is not
                // a class or a tuple of classes narrows nothing.
                ClassInfoConstraintFunction::IsInstance
                    .generate_constraint(
                        self.db,
                        &self.env,
                        inference.expression_type(classinfo),
                        true,
                        false,
                    )
                    .and_then(|instance| self.place_intersection(target, instance))
            }
            UnittestAssertion::True(target) => {
                self.evaluate_expression_node_predicate(target, expression, true)
            }
            UnittestAssertion::False(target) => {
                self.evaluate_expression_node_predicate(target, expression, false)
            }
        })
    }

    /// `target` narrowed to its intersection with `ty`, when `target` is a place (`ob`,
    /// `ob.data`).
    fn place_intersection(
        &mut self,
        target: &ast::Expr,
        ty: Type<'db>,
    ) -> Option<NarrowingConstraints<'db>> {
        let place = self.expect_place(&PlaceExpr::try_from_expr(target)?);
        Some(NarrowingConstraints::from_iter([(
            place,
            NarrowingConstraint::intersection(ty),
        )]))
    }
}
