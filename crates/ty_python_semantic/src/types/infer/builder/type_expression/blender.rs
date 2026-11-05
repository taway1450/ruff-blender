//! Calls in type expressions, where Blender's property declarations are taken over.
//!
//! `count: IntProperty(min=0)` is a call in an annotation, which the typing spec rejects. Blender
//! turns it into a property, so the annotation means the property's value type (`int`).

use ruff_python_ast as ast;

use crate::types::infer::builder::TypeInferenceBuilder;
use crate::types::{Type, TypeContext, blender};

impl<'db> TypeInferenceBuilder<'db, '_> {
    /// Infer the type expression `expression`, a call: the value type of a Blender property
    /// declaration, and an `invalid-type-form` for any other call.
    pub(super) fn infer_call_type_expression(
        &mut self,
        expression: &ast::Expr,
        call: &ast::ExprCall,
    ) -> Type<'db> {
        // Calls in a quoted annotation are not inferred, as the parsed string is not part of the
        // semantic index; only those that may declare a property are.
        if !self.in_string_annotation() || blender::may_be_property_call(call) {
            let callee = self.infer_callee(&call.func);
            self.infer_call_expression_impl(call, callee, TypeContext::default());
            let value_type = blender::annotation_value_type(
                self.db(),
                self.program_environment(),
                self.scope(),
                callee,
                call,
                |argument| self.expression_type(argument),
            );
            if let Some(value_type) = value_type {
                return value_type;
            }
        }
        self.report_invalid_type_expression(
            expression,
            format_args!(
                "Function calls are not allowed in {}s",
                self.type_expression_context()
            ),
        );
        Type::unknown()
    }
}
