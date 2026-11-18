//! Blender: a test case's assertion statement narrows the code after it, as an `assert` does.
//!
//! ```python
//! ob = bpy.data.objects.get("Cube")
//! self.assertIsNotNone(ob)  # as `assert ob is not None`
//! ob.name
//! ```
//!
//! `ty_blender::unittest_assertion` recognises the call; `ty_python_semantic`'s narrowing
//! (`types/dedicated/blender/assertion_narrowing.rs`) gives the constraint the predicate places.

use ruff_python_ast as ast;

use super::SemanticIndexBuilder;
use crate::expression::{Expression, ExpressionContext};

impl<'db, 'ast> SemanticIndexBuilder<'db, 'ast> {
    /// Records the call statement `value` as a narrowing predicate when it is a test case's
    /// assertion. `call_expr` is the call's standalone expression, which the predicate reuses.
    pub(super) fn record_unittest_assertion(
        &mut self,
        value: &'ast ast::Expr,
        call: &ast::ExprCall,
        call_expr: Expression<'db>,
        is_await: bool,
    ) {
        if !is_await && ty_blender::unittest_assertion(call).is_some() {
            let predicate = self.build_predicate(value, ExpressionContext::Condition, Some(call_expr));
            self.record_narrowing_constraint(predicate);
        }
    }
}
