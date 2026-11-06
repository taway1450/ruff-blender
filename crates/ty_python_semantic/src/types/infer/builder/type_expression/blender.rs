//! Calls in type expressions, where Blender's property declarations are taken over.
//!
//! `count: IntProperty(min=0)` is a call in an annotation, which the typing spec rejects. Blender
//! turns it into a property, so the annotation means the property's value type (`int`).

use ruff_python_ast as ast;
use ty_python_core::scope::ScopeKind;

use crate::types::infer::builder::TypeInferenceBuilder;
use crate::types::infer::nearest_enclosing_class;
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
            let declaration_callee = self.property_declaration_callee(callee, call);
            self.infer_call_expression_impl(call, declaration_callee, TypeContext::default());
            // The original callee says which `bpy.props` function this is.
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

    /// The callee to infer the call `callee(...)` against: for a property declaration in a class
    /// body, a `bpy.props` function whose callbacks take an instance of that class as `self`.
    fn property_declaration_callee(&self, callee: Type<'db>, call: &ast::ExprCall) -> Type<'db> {
        let db = self.db();
        let scope = self.scope();
        if scope.scope(db).kind() != ScopeKind::Class {
            return callee;
        }
        let Some(owner) = nearest_enclosing_class(db, self.index, scope) else {
            return callee;
        };
        // `poll`'s candidate is an instance of the `type=` class, which has to be known before the
        // call is inferred, so `type` (a keyword, or the first positional argument) is read only
        // when there is a `poll=`. A speculative builder infers it and is then discarded,
        // diagnostics included; the call infers the argument again as usual.
        let target = call
            .arguments
            .find_keyword("poll")
            .and(call.arguments.find_argument_value("type", 0))
            .and_then(|target| {
                let mut speculative = self.speculate();
                match speculative.infer_expression(target, TypeContext::default()) {
                    Type::ClassLiteral(class) => class.as_static(),
                    _ => None,
                }
            });
        blender::declaration_callee(db, self.program_environment(), callee, owner, target)
    }
}
