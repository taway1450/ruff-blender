//! Property callbacks: the `self` that Blender passes to `update=`, `get=`, `poll=` and the rest.
//!
//! Blender calls every callback of a property with the instance of the class that owns the
//! property as its first argument, and `poll` with an instance of the `type=` class as its second:
//!
//! ```python
//! class Settings(PropertyGroup):
//!     count: IntProperty(update=lambda self, context: ...)  # `self` is a `Settings`
//!     target: PointerProperty(type=Object, poll=lambda self, obj: obj.type == "MESH")  # `obj` is an `Object`
//! ```
//!
//! The `bpy` stubs cannot express this. Blender's own stubs type the first parameter as
//! `bpy_struct`, so a callback annotated `self: Settings` is rejected. fake-bpy-module's stubs make
//! it a type variable, which ty does not solve before it infers a lambda passed for it. So when
//! type-expression inference reads a property declaration in a class body, it infers the call
//! against [`declaration_callee`]: the `bpy.props` function with those parameters rewritten. A
//! lambda then takes its parameter types from the rewritten `Callable`, the way ty types any
//! lambda passed as an argument, and a `def` callback is checked against it.

use crate::types::callable::CallableTypeKind;
use crate::types::signatures::CallableSignature;
use crate::types::{CallableType, Parameter, Parameters, Signature, StaticClassLiteral, Type};
use crate::{Db, ProgramEnvironment};

use super::properties::is_property_function;

/// The parameters of the `bpy.props` functions that take a callback.
const CALLBACK_PARAMETERS: [&str; 8] = [
    "update",
    "get",
    "set",
    "get_transform",
    "set_transform",
    "search",
    "items",
    "poll",
];

/// The callee to infer the property declaration `callee(...)` against, in the body of `owner`.
///
/// When `callee` is one of the ten `bpy.props` functions, this is a callable with the same
/// signature, except that every `Callable` its callback parameters accept takes an `owner` as its
/// first parameter. `poll`'s callable also takes a `target` (the `type=` class) as its second,
/// when that is known. The stub's other parameter types and return types are kept, so a `get`
/// returning the wrong type is still an error. Any other callee is returned unchanged.
///
/// The generic context is kept. fake-bpy-module's stubs make the callbacks' first parameter a
/// type variable, which no longer occurs once it is rewritten; `poll`'s candidate is another one,
/// which stays and is solved as before when `type=` is unknown.
pub(crate) fn declaration_callee<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    callee: Type<'db>,
    owner: StaticClassLiteral<'db>,
    target: Option<StaticClassLiteral<'db>>,
) -> Type<'db> {
    let Type::FunctionLiteral(function) = callee else {
        return callee;
    };
    if !is_property_function(db, callee) {
        return callee;
    }
    let owner = Type::instance(db, env, owner.default_specialization(db));
    let target = target.map(|target| Type::instance(db, env, target.default_specialization(db)));

    let signature = function.signature(db);
    let overloads: Vec<_> = signature
        .iter()
        .map(|signature| {
            map_standard_parameters(signature, |_, parameter| {
                let name = parameter
                    .name()
                    .filter(|name| CALLBACK_PARAMETERS.contains(&name.as_str()))?;
                let mut first_parameters = vec![owner];
                if name == "poll"
                    && let Some(target) = target
                {
                    first_parameters.push(target);
                }
                Some(with_first_parameters(
                    db,
                    env,
                    parameter.annotated_type(),
                    &first_parameters,
                ))
            })
        })
        .collect();

    if overloads.iter().eq(signature.iter()) {
        return callee;
    }
    Type::Callable(CallableType::new(
        db,
        CallableSignature::from_overloads(overloads),
        CallableTypeKind::FunctionLike,
    ))
}

/// `ty` with the leading parameters of each `Callable` in it (`Callable[[bpy_struct, Context],
/// None] | None`) replaced by `first_parameters`, in order. Any other element of a union, such as
/// the static items of an `EnumProperty`, is kept.
fn with_first_parameters<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    first_parameters: &[Type<'db>],
) -> Type<'db> {
    match ty {
        Type::Union(union) => union.map(db, env, |element| {
            with_first_parameters(db, env, *element, first_parameters)
        }),
        Type::Callable(callable) => {
            let overloads = callable.signatures(db).iter().map(|signature| {
                map_standard_parameters(signature, |index, parameter| {
                    first_parameters
                        .get(index)
                        .copied()
                        .filter(|_| parameter.is_positional())
                })
            });
            Type::Callable(
                callable.with_signatures(db, CallableSignature::from_overloads(overloads)),
            )
        }
        _ => ty,
    }
}

/// `signature` with the type of each parameter for which `new_type` gives one replaced by it.
/// A signature whose parameters are not a standard list (`*args: P.args`, `...`) is kept.
fn map_standard_parameters<'db>(
    signature: &Signature<'db>,
    mut new_type: impl FnMut(usize, &Parameter<'db>) -> Option<Type<'db>>,
) -> Signature<'db> {
    if !signature.parameters().is_standard() {
        return signature.clone();
    }
    let parameters = signature
        .parameters()
        .iter()
        .enumerate()
        .map(|(index, parameter)| match new_type(index, parameter) {
            Some(ty) if ty != parameter.annotated_type() => {
                parameter.clone().with_annotated_type(ty)
            }
            _ => parameter.clone(),
        });
    Signature::new_generic(
        signature.generic_context,
        Parameters::standard(parameters.collect::<Vec<_>>()),
        signature.return_ty,
    )
    .with_definition(signature.definition)
}
