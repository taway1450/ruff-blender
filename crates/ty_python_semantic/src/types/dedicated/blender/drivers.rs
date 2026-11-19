//! Driver narrowing: `driver_add(path)` returns one F-Curve when `path` names a property of the
//! struct that is no array.
//!
//! ```python
//! bone.driver_add("bbone_segments")   # `_DriverFCurve`: an `int`
//! ob.driver_add("location")           # `list[_DriverFCurve]`: one per item of the array
//! ```
//!
//! The stubs type the call with `index=-1` as `_DriverFCurve | list[_DriverFCurve]`, as Blender
//! returns a list for every item of an array property (`pyrna_struct_driver_add`) and one F-Curve
//! otherwise. When the path is a literal naming a property of the struct itself (no `.` or `[`)
//! whose type is a number, a boolean or a string (an enum), the call returns the F-Curve alone. A
//! path that is not a literal, reaches into another struct, or names an array keeps the union.

use ruff_python_ast::{self as ast, Expr};

use crate::types::call::Bindings;
use crate::types::context::InferContext;
use crate::types::{KnownClass, KnownUnion, Type, UnionType};
use crate::{Db, ProgramEnvironment};

use super::symbols::blender_module_name;
use super::type_tags::string_literals;

/// Narrows the return type of `driver_add(path)` to one F-Curve when `path` names a property of the
/// receiver that is no array.
pub(super) fn on_call<'db>(
    context: &InferContext<'db, '_>,
    call: &ast::ExprCall,
    bindings: &mut Bindings<'db>,
    expression_type: &dyn Fn(&Expr) -> Option<Type<'db>>,
) {
    let db = context.db();
    let env = context.program_environment();
    let Some(path) = call
        .arguments
        .find_argument_value("path", 0)
        .and_then(expression_type)
        .and_then(|ty| string_literals(db, ty))
        .and_then(|paths| match &paths[..] {
            [path] => Some(path.clone()),
            _ => None,
        })
    else {
        return;
    };
    if path.contains(['.', '[']) {
        return;
    }
    for callable in bindings.iter_flat_mut() {
        let Type::BoundMethod(method) = callable.callable_type else {
            continue;
        };
        let Some(function) = method.function(db) else {
            continue;
        };
        if function.name(db) != "driver_add"
            || blender_module_name(db, function.program_file(db)) != Some("bpy.types")
        {
            continue;
        }
        let Some(property) = method
            .self_instance(db)
            .member(db, env, &path)
            .place
            .ignore_possibly_undefined()
        else {
            continue;
        };
        if !is_scalar(db, env, property) {
            continue;
        }
        for (_, binding) in callable.matching_overloads_mut() {
            if let Some(single) = without_lists(db, env, binding.return_ty) {
                binding.set_return_type(single);
            }
        }
    }
}

/// Whether a property of type `ty` is no array: a number, a boolean or a string.
fn is_scalar<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> bool {
    !ty.is_never()
        && (ty.is_assignable_to(db, env, KnownUnion::Float.to_type(db, env))
            || ty.is_assignable_to(db, env, KnownClass::Bool.to_instance(db, env))
            || ty.is_assignable_to(db, env, KnownClass::Str.to_instance(db, env)))
}

/// The elements of the union `ty` that are no list. `None` when `ty` is not a union with a list.
fn without_lists<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<Type<'db>> {
    let Type::Union(union) = ty else {
        return None;
    };
    let list = KnownClass::List.to_instance(db, env);
    let (lists, others): (Vec<Type<'db>>, Vec<Type<'db>>) = union
        .elements(db)
        .iter()
        .partition(|element| element.is_assignable_to(db, env, list));
    (!lists.is_empty() && !others.is_empty()).then(|| UnionType::from_elements(db, env, others))
}
