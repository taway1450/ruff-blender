//! Blender's property declarations in type expressions.
//!
//! `count: IntProperty(min=0)` is a call in an annotation, which the typing spec rejects. Blender
//! evaluates the annotation and turns the deferred property it gives into a property, so the
//! annotation means the property's value type (`int`). The same holds for anything that evaluates
//! to one: `count: COMMON`, `count: PROPS["count"]`, `count: wrapper()`. The Property evaluator
//! (`types::blender`) reads which declarations these are.

use ruff_python_ast as ast;
use ruff_text_size::Ranged;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::scope::ScopeKind;

use crate::types::infer::InferenceRegion;
use crate::types::infer::builder::TypeInferenceBuilder;
use crate::types::infer::nearest_enclosing_class;
use crate::types::{Type, TypeContext, blender};

impl<'db> TypeInferenceBuilder<'db, '_> {
    /// The type expression `reference`, a name or dotted name whose value ty inferred as `ty`, when
    /// that value is a deferred property (`count: COMMON`): the property's value type, or `Any`
    /// when the evaluator cannot tell which property it is. `None` for every other value.
    pub(super) fn blender_reference_type_expression(
        &self,
        ty: Type<'db>,
        reference: &ast::Expr,
    ) -> Option<Type<'db>> {
        blender::reference_value_type(
            self.db(),
            self.program_environment(),
            self.scope(),
            ty,
            reference,
        )
    }

    /// The type expression `subscript` of a container `value_ty` that holds deferred properties
    /// (`weight: PROPS["weight"]`): the value type of the property it picks, or `Any` when the
    /// evaluator cannot tell which. `None` for every other subscript.
    pub(super) fn blender_subscript_type_expression(
        &mut self,
        subscript: &ast::ExprSubscript,
        value_ty: Type<'db>,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let env = self.program_environment();
        if !blender::is_property_container(db, env, value_ty) {
            return None;
        }
        // Every expression of the file gets a type; the slice is a plain value here.
        if !self.in_string_annotation() {
            self.infer_expression(&subscript.slice, TypeContext::default());
        }
        Some(blender::subscript_value_type(
            db,
            env,
            self.scope(),
            subscript,
        ))
    }

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
            // `x: getattr(bpy.props, name)()`: Blender accepts any deferred property, and this
            // may be one, so the member is a property of any type rather than an error.
            if self.is_class_member_annotation(call)
                && let Some(class) = nearest_enclosing_class(self.db(), self.index, self.scope())
                && blender::is_opaque_call(self.db(), self.program_file(), class, callee)
            {
                return Type::any();
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

impl TypeInferenceBuilder<'_, '_> {
    /// Whether `call` is the whole annotation of a member in a class body (`x: call()`), not a part
    /// of one (`x: list[call()]`) or a parameter's or variable's annotation elsewhere.
    fn is_class_member_annotation(&self, call: &ast::ExprCall) -> bool {
        let db = self.db();
        if self.scope().scope(db).kind() != ScopeKind::Class {
            return false;
        }
        let (InferenceRegion::Definition(definition) | InferenceRegion::Deferred(definition)) =
            self.region
        else {
            return false;
        };
        let DefinitionKind::AnnotatedAssignment(assignment) = definition.kind(db) else {
            return false;
        };
        assignment.annotation(self.module()).range() == call.range()
    }
}
