//! The Add-on key evaluator: which add-on a key of `preferences.addons[KEY]`, or the `bl_idname` of
//! an `AddonPreferences` class, names.
//!
//! Blender keys an add-on by the name of the module it imports: the folder name of a legacy add-on,
//! `bl_ext.<repository>.<id>` for an extension. An extension's `__package__` is only `str` to ty
//! (the repository is the user's), so a key is decided by where its value comes from rather than by
//! its type: `__package__` of the root module and `__name__.rpartition(".")[0]` of a direct
//! submodule both name the root package, whatever the repository.
//!
//! Names are followed through their definitions, never by inferring add-on code, so the evaluator
//! is safe to call from attribute inference and from the Add-on index.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, Expr};
use ty_blender::{Addon, AddonKind};
use ty_module_resolver::{ImportingFile, ModuleName};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, global_scope, semantic_index};

use crate::Db;
use crate::types::definition_resolution::find_symbol_in_scope;

use super::addon_of_file;
use super::follow::{Resolved, module_by_name, resolve_expression};

/// What a key evaluates to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AddonKey {
    /// A module name, as a path below an add-on's root: `[]` is the root package itself,
    /// `["helpers"]` is `<root>.helpers`.
    Module { addon: Addon, path: Vec<String> },
    /// A string literal.
    Literal(String),
    /// Anything else: a parameter, a call, a module outside every add-on.
    Unknown,
}

impl AddonKey {
    /// Whether the key is the name Blender knows `addon` by: its root package, or for a legacy
    /// add-on also its name as a literal. An extension's name is never a literal.
    pub(super) fn names(&self, addon: &Addon) -> bool {
        match self {
            AddonKey::Module { addon: owner, path } => owner == addon && path.is_empty(),
            AddonKey::Literal(literal) => {
                addon.kind != AddonKind::Extension && *literal == addon.name
            }
            AddonKey::Unknown => false,
        }
    }
}

/// How many names and imports are followed at most, which also stops cycles.
const MAX_DEPTH: usize = 16;

/// The key `expression` evaluates to in `scope`.
pub(super) fn expression_key<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    expression: &Expr,
) -> AddonKey {
    expression_key_at_depth(db, scope, expression, 0)
}

/// The key the name that `definition` binds gives (`bl_idname = __package__`).
pub(super) fn definition_key<'db>(db: &'db dyn Db, definition: Definition<'db>) -> AddonKey {
    definition_key_at_depth(db, definition, 0)
}

fn expression_key_at_depth<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    expression: &Expr,
    depth: usize,
) -> AddonKey {
    if depth > MAX_DEPTH {
        return AddonKey::Unknown;
    }
    match expression {
        Expr::StringLiteral(string) => AddonKey::Literal(string.value.to_str().to_string()),
        Expr::Name(name) => name_key(db, scope, name.id.as_str(), depth),
        // `module.NAME`, with `module` a module.
        Expr::Attribute(attribute) => match resolve_expression(db, scope, &attribute.value) {
            Some(Resolved::Module(module)) => name_key(
                db,
                global_scope(db, module),
                attribute.attr.as_str(),
                depth + 1,
            ),
            _ => AddonKey::Unknown,
        },
        // `e.rpartition(".")[0]`: the package of the module `e` names.
        Expr::Subscript(subscript) => {
            let Some(partitioned) = rpartition_base(subscript) else {
                return AddonKey::Unknown;
            };
            match expression_key_at_depth(db, scope, partitioned, depth + 1) {
                AddonKey::Module { addon, mut path } if !path.is_empty() => {
                    path.pop();
                    AddonKey::Module { addon, path }
                }
                _ => AddonKey::Unknown,
            }
        }
        _ => AddonKey::Unknown,
    }
}

/// `e` of `e.rpartition(".")[0]`.
fn rpartition_base(subscript: &ast::ExprSubscript) -> Option<&Expr> {
    let Expr::NumberLiteral(ast::ExprNumberLiteral {
        value: ast::Number::Int(index),
        ..
    }) = &*subscript.slice
    else {
        return None;
    };
    if index.as_u8() != Some(0) {
        return None;
    }
    let Expr::Call(call) = &*subscript.value else {
        return None;
    };
    let Expr::Attribute(method) = &*call.func else {
        return None;
    };
    let [Expr::StringLiteral(separator)] = &*call.arguments.args else {
        return None;
    };
    (method.attr.as_str() == "rpartition"
        && call.arguments.keywords.is_empty()
        && separator.value.to_str() == ".")
        .then_some(&*method.value)
}

/// The key the name `name` gives in `scope`: its bindings in the nearest enclosing scope that has
/// any, which must all agree. `__name__` and `__package__` with no binding are the module's own.
fn name_key<'db>(db: &'db dyn Db, scope: ScopeId<'db>, name: &str, depth: usize) -> AddonKey {
    let file = scope.program_file(db);
    let index = semantic_index(db, file);
    let definitions = index
        .visible_ancestor_scopes(scope.file_scope_id(db))
        .map(|(ancestor, _)| find_symbol_in_scope(db, ancestor.to_scope_id(db, file), name))
        .find(|definitions| !definitions.is_empty());
    let Some(definitions) = definitions else {
        return dunder_key(db, file, name);
    };
    let mut keys = definitions
        .iter()
        .map(|definition| definition_key_at_depth(db, *definition, depth + 1));
    let Some(first) = keys.next() else {
        return AddonKey::Unknown;
    };
    if keys.all(|key| key == first) {
        first
    } else {
        AddonKey::Unknown
    }
}

fn definition_key_at_depth<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    depth: usize,
) -> AddonKey {
    if depth > MAX_DEPTH {
        return AddonKey::Unknown;
    }
    let module = parsed_module(db, definition.python_file(db)).load(db);
    match definition.kind(db) {
        DefinitionKind::Assignment(assignment) if assignment.target(&module).is_name_expr() => {
            expression_key_at_depth(db, definition.scope(db), assignment.value(&module), depth)
        }
        DefinitionKind::AnnotatedAssignment(assignment)
            if assignment.target(&module).is_name_expr() =>
        {
            assignment
                .value(&module)
                .map_or(AddonKey::Unknown, |value| {
                    expression_key_at_depth(db, definition.scope(db), value, depth)
                })
        }
        // `from . import ADDON_ID`, `from .. import __package__ as base_package`: the name as the
        // module it comes from binds it. The implicit dunders are that module's own.
        DefinitionKind::ImportFrom(import) => {
            let file = definition.program_file(db);
            let importing = ImportingFile::ResolverFile(file.resolver_file(db));
            let Ok(source) =
                ModuleName::from_import_statement(db, importing, import.import(&module))
            else {
                return AddonKey::Unknown;
            };
            let Some(source) = module_by_name(db, file, source.as_str()) else {
                return AddonKey::Unknown;
            };
            name_key(
                db,
                global_scope(db, source),
                import.alias(&module).name.as_str(),
                depth,
            )
        }
        _ => AddonKey::Unknown,
    }
}

/// The module's own `__name__` or `__package__`, as Blender imports the add-on `file` is in.
fn dunder_key<'db>(db: &'db dyn Db, file: ProgramFile<'db>, name: &str) -> AddonKey {
    let package = match name {
        "__package__" => true,
        "__name__" => false,
        _ => return AddonKey::Unknown,
    };
    let Some(addon) = addon_of_file(db, file) else {
        return AddonKey::Unknown;
    };
    let Some(path) = file.file(db).path(db).as_system_path() else {
        return AddonKey::Unknown;
    };
    if addon.kind == AddonKind::SingleFile {
        // A top-level module: `__name__` is the add-on, `__package__` is empty.
        return if package {
            AddonKey::Literal(String::new())
        } else {
            AddonKey::Module {
                addon: addon.clone(),
                path: Vec::new(),
            }
        };
    }
    let Ok(relative) = path.strip_prefix(&addon.root) else {
        return AddonKey::Unknown;
    };
    let mut parts: Vec<String> = relative
        .components()
        .map(|component| component.as_str().to_string())
        .collect();
    let Some(last) = parts.pop() else {
        return AddonKey::Unknown;
    };
    let Some(stem) = last.strip_suffix(".py") else {
        return AddonKey::Unknown;
    };
    // A package's `__init__.py` is the package itself, its own `__package__` too.
    if stem != "__init__" && !package {
        parts.push(stem.to_string());
    }
    AddonKey::Module {
        addon: addon.clone(),
        path: parts,
    }
}
