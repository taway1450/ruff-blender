//! PROTOTYPE (wayfinder ticket #10, throwaway): Blender property annotations.
//!
//! `count: IntProperty()` in a class body is taken over in the `Call` arm of type-expression
//! inference (ticket #4), and the class-member paths ask this module whether a member is a
//! Blender property. Two routes answer that question; see `member.rs`.

mod member;

pub(crate) use member::*;

use ruff_python_ast as ast;
use ty_module_resolver::file_to_module;

use crate::Db;
use crate::ProgramEnvironment;
use crate::types::class::{ClassLiteral, StaticClassLiteral};
use crate::types::function::FunctionType;
use crate::types::{KnownClass, Type, UnionType};

/// The ten `bpy.props` declaration functions (`RemoveProperty` is not one).
const PROPERTY_FUNCTIONS: [&str; 10] = [
    "BoolProperty",
    "BoolVectorProperty",
    "CollectionProperty",
    "EnumProperty",
    "FloatProperty",
    "FloatVectorProperty",
    "IntProperty",
    "IntVectorProperty",
    "PointerProperty",
    "StringProperty",
];

/// Classes whose annotations Blender turns into properties (Blender 5.4, `props_hosts`).
const PROPERTY_HOSTS: [&str; 9] = [
    "PropertyGroup",
    "Operator",
    "Macro",
    "AddonPreferences",
    "UIList",
    "Node",
    "NodeSocket",
    "NodeTree",
    "Gizmo",
];

/// Syntactic pre-filter: could this call be a `bpy.props` declaration? Pure AST, so it can gate
/// inferring a call inside a quoted annotation without changing stock ty for `"f()"`.
pub(crate) fn may_be_property_call(call: &ast::ExprCall) -> bool {
    callee_name(&call.func).is_some_and(|name| PROPERTY_FUNCTIONS.contains(&name))
}

fn callee_name(func: &ast::Expr) -> Option<&str> {
    match func {
        ast::Expr::Name(name) => Some(name.id.as_str()),
        ast::Expr::Attribute(attr) => Some(attr.attr.id.as_str()),
        _ => None,
    }
}

/// The property function a callee type stands for, recognised by its defining module.
pub(crate) fn property_function<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<&'static str> {
    let Type::FunctionLiteral(function) = callee else {
        return None;
    };
    is_bpy_props(db, function)
        .then(|| {
            PROPERTY_FUNCTIONS
                .into_iter()
                .find(|name| function.name(db).as_str() == *name)
        })
        .flatten()
}

fn is_bpy_props(db: &dyn Db, function: FunctionType<'_>) -> bool {
    file_to_module(db, function.program_file(db).resolver_file(db))
        .is_some_and(|module| module.name(db).as_str() == "bpy.props")
}

fn is_bpy_types_class(db: &dyn Db, class: ClassLiteral<'_>) -> bool {
    file_to_module(db, class.program_file(db).resolver_file(db))
        .is_some_and(|module| module.name(db).as_str().starts_with("bpy.types"))
}

/// Hook in the `Call` arm: the value type of a property declaration used as an annotation, or
/// `None` for any other call. `arg_ty` reads the argument types the arm has just inferred.
pub(crate) fn annotation_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    callee: Type<'db>,
    call: &ast::ExprCall,
    arg_ty: &dyn Fn(&ast::Expr) -> Option<Type<'db>>,
) -> Option<Type<'db>> {
    let kind = property_function(db, callee)?;
    Some(value_type(db, env, kind, call, arg_ty))
}

/// Is `class` (or a base) one of the classes whose annotations become properties?
pub(crate) fn is_property_host(db: &dyn Db, class: StaticClassLiteral<'_>) -> bool {
    class.iter_mro(db, None).any(|base| {
        base.into_class().is_some_and(|base| {
            let base = base.class_literal(db);
            PROPERTY_HOSTS.contains(&base.name(db).as_str()) && is_bpy_types_class(db, base)
        })
    })
}

fn keyword<'a>(call: &'a ast::ExprCall, name: &str) -> Option<&'a ast::Expr> {
    call.arguments
        .keywords
        .iter()
        .find(|k| k.arg.as_ref().is_some_and(|arg| arg.as_str() == name))
        .map(|k| &k.value)
}

/// Rough value-type table (ticket #7); vectors and collections are stand-ins.
fn value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    kind: &str,
    call: &ast::ExprCall,
    arg_ty: &dyn Fn(&ast::Expr) -> Option<Type<'db>>,
) -> Type<'db> {
    match kind {
        "BoolProperty" => KnownClass::Bool.to_instance(db, env),
        "IntProperty" => KnownClass::Int.to_instance(db, env),
        // The annotation `float` means `int | float`.
        "FloatProperty" => UnionType::from_elements(
            db,
            env,
            [
                KnownClass::Int.to_instance(db, env),
                KnownClass::Float.to_instance(db, env),
            ],
        ),
        "StringProperty" => {
            let bytes = matches!(keyword(call, "subtype"),
                Some(ast::Expr::StringLiteral(s)) if s.value.to_str() == "BYTE_STRING");
            if bytes {
                KnownClass::Bytes.to_instance(db, env)
            } else {
                KnownClass::Str.to_instance(db, env)
            }
        }
        "EnumProperty" => {
            let literals = match keyword(call, "items") {
                Some(ast::Expr::List(ast::ExprList { elts, .. }))
                | Some(ast::Expr::Tuple(ast::ExprTuple { elts, .. })) => elts
                    .iter()
                    .filter(|e| !e.is_none_literal_expr())
                    .map(|e| match e {
                        ast::Expr::Tuple(t) => match t.elts.first() {
                            Some(ast::Expr::StringLiteral(s)) => {
                                Some(Type::string_literal(db, s.value.to_str()))
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>(),
                _ => None,
            };
            match literals {
                Some(literals) if !literals.is_empty() => {
                    UnionType::from_elements(db, env, literals)
                }
                _ => KnownClass::Str.to_instance(db, env),
            }
        }
        "PointerProperty" => {
            let Some(Type::ClassLiteral(target)) = keyword(call, "type").and_then(arg_ty) else {
                return Type::unknown();
            };
            let instance = Type::instance(db, env, target.default_specialization(db));
            let is_id = target.iter_mro(db).any(|base| {
                base.into_class().is_some_and(|base| {
                    let base = base.class_literal(db);
                    base.name(db).as_str() == "ID" && is_bpy_types_class(db, base)
                })
            });
            if is_id {
                UnionType::from_elements(db, env, [instance, Type::none(db, env)])
            } else {
                instance
            }
        }
        // Vectors and collections: out of this prototype's scope.
        _ => Type::unknown(),
    }
}
