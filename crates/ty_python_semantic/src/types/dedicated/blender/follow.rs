//! The shared follower: what a name or dotted name refers to, found through the definitions of its
//! names rather than through inference.
//!
//! The Add-on index uses it to find the owner and the callee of a registration without inferring
//! `register()`, and the Property evaluator uses it to find the constant an annotation names
//! (`x: COMMON`, `x: props.COMMON`).

use ruff_db::parsed::parsed_module;
use ruff_python_ast::Expr;
use ty_module_resolver::{ModuleName, file_to_module, resolve_module_confident};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, global_scope, place_table, semantic_index};

use crate::types::definition_resolution::{
    self, ImportAliasResolution, ResolvedDefinition, find_symbol_in_scope,
};
use crate::types::{Type, binding_type};
use crate::{Db, ProgramEnvironment};

/// What a name or dotted name refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resolved<'db> {
    Module(ProgramFile<'db>),
    /// The type of a class or function definition.
    Value(Type<'db>),
    /// An assignment to a name whose value is anything else (`COMMON = IntProperty()`), to be
    /// read by whoever asked.
    Constant(Definition<'db>),
}

/// How many assignments (`T = bpy.types`) are followed at most, which also stops alias cycles.
const MAX_ALIAS_DEPTH: usize = 8;

/// Resolve `expression`, evaluated in `scope`, to a module, class, function or constant by
/// following the definitions of its names: imports (including star imports and re-exports through
/// other modules), aliases and module attributes. Only the types of class and function definitions
/// are inferred. `None` when a name has several different definitions or anything else is involved.
pub(super) fn resolve_expression<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    expression: &Expr,
) -> Option<Resolved<'db>> {
    resolve_expression_at_depth(db, scope, expression, 0)
}

fn resolve_expression_at_depth<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    expression: &Expr,
    depth: usize,
) -> Option<Resolved<'db>> {
    match expression {
        Expr::Name(name) => resolve_name(db, scope, name.id.as_str(), depth),
        Expr::Attribute(attribute) => {
            let Resolved::Module(module) =
                resolve_expression_at_depth(db, scope, &attribute.value, depth)?
            else {
                return None;
            };
            resolve_module_member(db, module, attribute.attr.id.as_str(), depth)
        }
        _ => None,
    }
}

/// The name `name` in `scope`: its definitions in the nearest enclosing scope that has any, which
/// must all resolve to the same thing.
fn resolve_name<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    name: &str,
    depth: usize,
) -> Option<Resolved<'db>> {
    let file = scope.program_file(db);
    let index = semantic_index(db, file);
    let definitions = index
        .visible_ancestor_scopes(scope.file_scope_id(db))
        .map(|(ancestor, _)| find_symbol_in_scope(db, ancestor.to_scope_id(db, file), name))
        .find(|definitions| !definitions.is_empty())?;
    all_same(
        definitions
            .iter()
            .map(|definition| resolve_binding(db, *definition, name, depth)),
    )
}

/// What the binding `definition` of `name` refers to.
fn resolve_binding<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    name: &str,
    depth: usize,
) -> Option<Resolved<'db>> {
    let file = definition.program_file(db);
    match definition.kind(db) {
        // `import bpy.props` binds `bpy`, while `import bpy.props as p` binds `bpy.props`. (ty's
        // navigation resolves both to `bpy.props`.)
        DefinitionKind::Import(import) => {
            let module = parsed_module(db, definition.python_file(db)).load(db);
            let alias = import.alias(&module);
            let module_name = if alias.asname.is_some() {
                ModuleName::new(&alias.name)?
            } else {
                ModuleName::new(alias.name.split('.').next()?)?
            };
            resolve_module_file(db, file, &module_name)
        }
        DefinitionKind::ImportFromSubmodule(_) => imported_submodule(db, definition),
        _ => {
            let env = ProgramEnvironment::from_file(file);
            all_same(
                definition_resolution::resolve_definition(
                    db,
                    &env,
                    definition,
                    Some(name),
                    ImportAliasResolution::ResolveAliases,
                )
                .iter()
                .map(|resolved| resolve_definition(db, resolved, depth)),
            )
        }
    }
}

/// The one item of `items`, when they are all the same and none is `None`.
fn all_same<'db>(mut items: impl Iterator<Item = Option<Resolved<'db>>>) -> Option<Resolved<'db>> {
    let first = items.next()??;
    items.all(|item| item == Some(first)).then_some(first)
}

/// `member` of `module`: a name the module defines or imports, or else its submodule.
fn resolve_module_member<'db>(
    db: &'db dyn Db,
    module: ProgramFile<'db>,
    member: &str,
    depth: usize,
) -> Option<Resolved<'db>> {
    resolve_name(db, global_scope(db, module), member, depth)
        .or_else(|| submodule(db, module, member))
}

/// The submodule that `definition` binds: `from .types import Scene` (or `from bpy.types import
/// Scene`) in `bpy/__init__.py` also binds `types` to the submodule `bpy.types`.
fn imported_submodule<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Option<Resolved<'db>> {
    let table = place_table(db, definition.scope(db));
    let ScopedPlaceId::Symbol(symbol) = definition.place(db) else {
        return None;
    };
    submodule(
        db,
        definition.program_file(db),
        table.symbol(symbol).name().as_str(),
    )
}

/// The submodule `name` of the package `package`.
fn submodule<'db>(db: &'db dyn Db, package: ProgramFile<'db>, name: &str) -> Option<Resolved<'db>> {
    let mut module_name = file_to_module(db, package.resolver_file(db))?
        .name(db)
        .clone();
    module_name.extend(&ModuleName::new(name)?);
    resolve_module_file(db, package, &module_name)
}

/// The module `name`, as `importing_file` would import it.
fn resolve_module_file<'db>(
    db: &'db dyn Db,
    importing_file: ProgramFile<'db>,
    name: &ModuleName,
) -> Option<Resolved<'db>> {
    let env = ProgramEnvironment::from_file(importing_file);
    let module = resolve_module_confident(db, env.resolver_environment(db), name)?;
    Some(Resolved::Module(ProgramFile::new(
        db,
        module.file(db)?,
        importing_file.program(db),
    )))
}

fn resolve_definition<'db>(
    db: &'db dyn Db,
    definition: &ResolvedDefinition<'db>,
    depth: usize,
) -> Option<Resolved<'db>> {
    let definition = match definition {
        ResolvedDefinition::Module(module) => return Some(Resolved::Module(*module)),
        ResolvedDefinition::FileWithRange(_) => return None,
        ResolvedDefinition::Definition(definition) => *definition,
    };
    match definition.kind(db) {
        DefinitionKind::Class(_) | DefinitionKind::Function(_) => {
            Some(Resolved::Value(binding_type(db, definition)))
        }
        // ty's resolution leaves these as they are, as in `from bpy import types as types` of a
        // `bpy/__init__.pyi` that also has `from bpy.types import Context`.
        DefinitionKind::ImportFromSubmodule(_) => imported_submodule(db, definition),
        // `T = bpy.types` or `Group = bpy.types.PropertyGroup`: follow the assigned value. Any
        // other value (`COMMON = IntProperty()`) is a constant.
        DefinitionKind::Assignment(assignment) if depth < MAX_ALIAS_DEPTH => {
            let module = parsed_module(db, definition.python_file(db)).load(db);
            if !assignment.target(&module).is_name_expr() {
                return None;
            }
            resolve_expression_at_depth(
                db,
                definition.scope(db),
                assignment.value(&module),
                depth + 1,
            )
            .or(Some(Resolved::Constant(definition)))
        }
        // `SETTINGS: dict[str, Any] = {...}`: its value is not followed as an alias.
        DefinitionKind::AnnotatedAssignment(assignment) => {
            let module = parsed_module(db, definition.python_file(db)).load(db);
            (assignment.target(&module).is_name_expr() && assignment.value(&module).is_some())
                .then_some(Resolved::Constant(definition))
        }
        _ => None,
    }
}
