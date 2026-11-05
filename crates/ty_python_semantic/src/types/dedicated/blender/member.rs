//! Route A: the annotation fallback arm marks a property declaration with the non-standard
//! `BLENDER_PROPERTY` qualifier; the member paths read it back from the declaration.

use ruff_python_ast as ast;

use crate::Db;
use crate::types::class::StaticClassLiteral;
use crate::types::{Type, TypeQualifiers};

use super::{is_property_host, property_function};

/// Hook in the annotation fallback arm: is this annotation a property declaration? Reads the
/// callee type the `Call` arm has just stored.
pub(crate) fn is_property_annotation<'db>(
    db: &'db dyn Db,
    annotation: &ast::Expr,
    expr_ty: &dyn Fn(&ast::Expr) -> Option<Type<'db>>,
) -> bool {
    let ast::Expr::Call(call) = annotation else {
        return false;
    };
    expr_ty(&call.func).is_some_and(|callee| property_function(db, callee).is_some())
}

/// Hook in `own_class_member`: a property is not readable on the class object.
pub(crate) fn hides_class_member(
    _db: &dyn Db,
    _class: StaticClassLiteral<'_>,
    _name: &str,
    qualifiers: TypeQualifiers,
) -> bool {
    qualifiers.contains(TypeQualifiers::BLENDER_PROPERTY)
}

/// Hook in `own_instance_member`: off a property host, the annotation creates no attribute.
pub(crate) fn hides_instance_member(
    db: &dyn Db,
    class: StaticClassLiteral<'_>,
    _name: &str,
    qualifiers: TypeQualifiers,
) -> bool {
    qualifiers.contains(TypeQualifiers::BLENDER_PROPERTY) && !is_property_host(db, class)
}
