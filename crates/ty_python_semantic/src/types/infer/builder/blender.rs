//! ty-blender's hooks into the inference builder, each a one-line call at its upstream site that
//! hands the builder's state to `types::blender`.

use ruff_python_ast as ast;
use ty_python_core::definition::Definition;

use crate::types::infer::builder::TypeInferenceBuilder;
use crate::types::{StaticClassLiteral, Type, TypeContext, blender};

impl<'db> TypeInferenceBuilder<'db, '_> {
    /// The type of the unannotated parameter `parameter`, whose default is `default`, in an
    /// override of a Blender callback (`blender::override_parameter_type`).
    pub(super) fn blender_parameter_type(
        &self,
        parameter: Definition<'db>,
        default: Option<&ast::Expr>,
    ) -> Option<Type<'db>> {
        let default = default.map(|default| self.file_expression_type(default));
        blender::override_parameter_type(self.db(), self.program_environment(), parameter, default)
    }

    /// The type of the subscript load `subscript`, which ty inferred as `ty`: an item of
    /// `bl_rna.properties` with a string-literal key is the `Property` subclass of the attribute's
    /// kind (`blender::rna_property_type`).
    pub(super) fn blender_subscript_type(
        &self,
        subscript: &ast::ExprSubscript,
        ty: Type<'db>,
    ) -> Type<'db> {
        blender::rna_property_type(
            self.db(),
            self.program_environment(),
            subscript,
            ty,
            |expression| self.expression_type(expression),
        )
        .unwrap_or(ty)
    }

    /// The type of `name`, unbound where it is read, when a module bound it on its previous run (the
    /// reload guard) or a package imports it by name (`blender::reload_guard_type`).
    pub(super) fn blender_reload_guard_type(&self, name: &ast::ExprName) -> Option<Type<'db>> {
        blender::reload_guard_type(self.db(), self.scope(), self.module(), name)
    }

    /// The callee to infer `call` against when it is the value of a property assigned to a class
    /// outside its body (`bpy.types.Armature.target = PointerProperty(poll=...)`): a `bpy.props`
    /// function whose callbacks take an instance of that class as `self`
    /// (`blender::declaration_callee`). Any other call keeps `callee`.
    pub(super) fn blender_assignment_callee(
        &self,
        call: &ast::ExprCall,
        callee: Type<'db>,
    ) -> Type<'db> {
        if !blender::is_property_function(self.db(), callee) {
            return callee;
        }
        let Some(owner) = blender::assigned_property_owner(self.module().syntax(), call) else {
            return callee;
        };
        let mut speculative = self.speculate();
        let Type::ClassLiteral(owner) = speculative.infer_expression(owner, TypeContext::default())
        else {
            return callee;
        };
        let Some(owner) = owner.as_static() else {
            return callee;
        };
        let target = self.blender_poll_target(call);
        blender::declaration_callee(self.db(), self.program_environment(), callee, owner, target)
    }

    /// The `type=` class of the property declaration `call` when it passes `poll=`: `poll`'s
    /// candidate is an instance of it, which has to be known before the call is inferred, so `type`
    /// (a keyword, or the first positional argument) is read only when there is a `poll=`. A
    /// speculative builder infers it and is then discarded, diagnostics included; the call infers
    /// the argument again as usual.
    pub(super) fn blender_poll_target(
        &self,
        call: &ast::ExprCall,
    ) -> Option<StaticClassLiteral<'db>> {
        call.arguments
            .find_keyword("poll")
            .and(call.arguments.find_argument_value("type", 0))
            .and_then(|target| {
                let mut speculative = self.speculate();
                match speculative.infer_expression(target, TypeContext::default()) {
                    Type::ClassLiteral(class) => class.as_static(),
                    _ => None,
                }
            })
    }
}
