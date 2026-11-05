//! Route B: a tracked query keyed on the member's `Definition` decides whether its annotation is a
//! property declaration: syntactic check of the annotation, then the callee is resolved through
//! the defining file's module globals. No core type changes.

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_parser::parse_expression;
use ty_python_core::definition::{Definition, DefinitionKind, DefinitionState};
use ty_python_core::program_file::ProgramFile;

use crate::Db;
use crate::ProgramEnvironment;
use crate::place::global_symbol;
use ty_python_core::{place_table, use_def_map};
use crate::types::Type;
use crate::types::class::StaticClassLiteral;

use super::{is_property_host, may_be_property_call, property_function};

/// Hook in `own_class_member`: a property is not readable on the class object.
pub(crate) fn hides_class_member(db: &dyn Db, class: StaticClassLiteral<'_>, name: &str) -> bool {
    declares_property(db, class, name)
}

/// Hook in `own_instance_member`: off a property host, the annotation creates no attribute.
pub(crate) fn hides_instance_member(
    db: &dyn Db,
    class: StaticClassLiteral<'_>,
    name: &str,
) -> bool {
    declares_property(db, class, name) && is_off_host(db, class)
}

/// A `bpy.types` subclass that is not a host (`Panel`, `Menu`, ...). A plain class (a mixin)
/// keeps the attribute: it becomes a property in whatever host class mixes it in.
fn is_off_host(db: &dyn Db, class: StaticClassLiteral<'_>) -> bool {
    let derives_from_bpy_types = class.iter_mro(db, None).skip(1).any(|base| {
        base.into_class()
            .is_some_and(|base| super::is_bpy_types_class(db, base.class_literal(db)))
    });
    derives_from_bpy_types && !is_property_host(db, class)
}

/// Does `class`'s own body declare `name` with a property annotation?
fn declares_property(db: &dyn Db, class: StaticClassLiteral<'_>, name: &str) -> bool {
    let scope = class.body_scope(db);
    let Some(symbol) = place_table(db, scope).symbol_id(name) else {
        return false;
    };
    use_def_map(db, scope)
        .end_of_scope_symbol_declarations(symbol)
        .any(|declaration| match declaration.declaration {
            DefinitionState::Defined(definition) => is_property_declaration(db, definition),
            _ => false,
        })
}

/// The query: is this definition `name: <bpy.props call>` (plain or quoted)?
#[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
fn is_property_declaration<'db>(db: &'db dyn Db, definition: Definition<'db>) -> bool {
    let DefinitionKind::AnnotatedAssignment(assignment) = definition.kind(db) else {
        return false;
    };
    let file = definition.program_file(db);
    let module = parsed_module(db, definition.python_file(db)).load(db);
    match assignment.annotation(&module) {
        ast::Expr::Call(call) => is_property_call(db, file, call),
        ast::Expr::StringLiteral(string) => {
            let Ok(parsed) = parse_expression(string.value.to_str()) else {
                return false;
            };
            match parsed.expr() {
                ast::Expr::Call(call) => is_property_call(db, file, call),
                _ => false,
            }
        }
        _ => false,
    }
}

fn is_property_call(db: &dyn Db, file: ProgramFile<'_>, call: &ast::ExprCall) -> bool {
    may_be_property_call(call)
        && resolve(db, file, &call.func).is_some_and(|callee| property_function(db, callee).is_some())
}

/// Resolve a dotted callee through the file's module globals (class-scope and function-local
/// imports are not seen; good enough for a prototype).
fn resolve<'db>(db: &'db dyn Db, file: ProgramFile<'db>, expr: &ast::Expr) -> Option<Type<'db>> {
    match expr {
        ast::Expr::Name(name) => global_symbol(db, file, &name.id).ignore_possibly_undefined(),
        ast::Expr::Attribute(attr) => {
            let env = ProgramEnvironment::from_file(file);
            resolve(db, file, &attr.value)?
                .member(db, &env, &attr.attr.id)
                .place
                .ignore_possibly_undefined()
        }
        _ => None,
    }
}
