//! ty-blender's hooks into the inference builder, each a one-line call at its upstream site that
//! hands the builder's state to `types::blender`.

use ruff_python_ast as ast;
use ty_python_core::definition::Definition;

use crate::types::infer::builder::TypeInferenceBuilder;
use crate::types::{Type, blender};

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

    /// The type of `name`, unbound where it is read, in the guarded branch of an add-on's reload
    /// guard (`blender::reload_guard_type`).
    pub(super) fn blender_reload_guard_type(&self, name: &ast::ExprName) -> Option<Type<'db>> {
        blender::reload_guard_type(self.db(), self.scope(), self.module(), name)
    }
}
