//! The return values of Blender callbacks that have no return annotation.
//!
//! Blender checks what a callback returns against its RNA definition when it calls it, and raises
//! when the value does not fit: `execute` must return a set of the operator's result names. An
//! unannotated override of a Blender callback has its parent's return type
//! (`types::blender::override_return_type`), so its returns are checked as if it were annotated:
//!
//! ```python
//! class MY_OT_run(Operator):
//!     def execute(self, context):
//!         return {"DONE"}  # error: [invalid-return-type]
//! ```

use ruff_python_ast as ast;
use ruff_text_size::Ranged;
use ty_python_core::scope::NodeWithScopeRef;

use crate::reachability::is_end_of_scope_reachable;
use crate::types::diagnostic::{report_implicit_return_type, report_invalid_return_type};
use crate::types::function::{FunctionBodyKind, function_body_kind};
use crate::types::infer::builder::TypeInferenceBuilder;
use crate::types::{Type, blender};

impl<'db> TypeInferenceBuilder<'db, '_> {
    /// Reports `invalid-return-type` on the returns of `function`, an override of a Blender
    /// callback without a return annotation, that do not fit the parent's return type, and on its
    /// name when the end of its body is reachable and `None` does not fit.
    ///
    /// Callbacks whose result Blender converts with `bool()` accept any value. A stub-like body
    /// (`...`, `pass`, a docstring) is left alone, as are async and generator functions, which
    /// Blender cannot call as callbacks.
    pub(super) fn check_blender_override_returns(&self, function: &ast::StmtFunctionDef) {
        let db = self.db();
        let env = self.program_environment();
        let definition = self.index.expect_single_definition(function);
        let Some(expected) = blender::override_return_type(db, definition) else {
            return;
        };
        if function.is_async
            || self
                .index
                .node_scope(NodeWithScopeRef::Function(function))
                .is_generator_function(self.index)
        {
            return;
        }
        let no_return = self.return_types_and_ranges.is_empty();
        if no_return
            && function_body_kind(db, env, function, |expr| self.expression_type(expr))
                == FunctionBodyKind::Stub
        {
            return;
        }
        let name = function.name.range();
        for statement in &self.return_types_and_ranges {
            if !statement.ty.is_assignable_to(db, env, expected) {
                report_invalid_return_type(
                    &self.context,
                    statement.range,
                    name,
                    expected,
                    statement.ty,
                );
            }
        }
        if is_end_of_scope_reachable(db, self.scope())
            && !Type::none(db, env).is_assignable_to(db, env, expected)
        {
            report_implicit_return_type(&self.context, name, expected, false, None, no_return);
        }
    }
}
