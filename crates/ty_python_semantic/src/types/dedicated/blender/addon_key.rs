//! The Add-on key evaluator: which add-on a key of `preferences.addons[KEY]`, or the `bl_idname` of
//! an `AddonPreferences` class, names.
//!
//! Blender keys an add-on by the name of the module it imports: the folder name of a legacy add-on,
//! `bl_ext.<repository>.<id>` for an extension. An extension's `__package__` is only `str` to ty
//! (the repository is the user's), so a key is decided by where its value comes from rather than by
//! its type: `__package__` of the root module and `__name__.rpartition(".")[0]` of a direct
//! submodule both name the root package, whatever the repository. The first component of a module
//! name (`__name__.split(".")[0]`) is a legacy add-on's name, but `bl_ext` for an extension.
//!
//! Names are followed through their definitions, never by inferring add-on code, so the evaluator
//! is safe to call from attribute inference and from the Add-on index. The key of each definition is
//! a query of its own ([`definition_key`]), so reading another module's syntax for it does not tie
//! the caller to that module's syntax tree.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, Expr};
use ty_blender::{Addon, AddonKind};
use ty_module_resolver::{ImportingFile, ModuleName};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, global_scope};

use crate::Db;

use super::addon_of_file;
use super::follow::{Resolved, agreed, module_by_name, nearest_definitions, resolve_expression};

/// What a key evaluates to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
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

/// The key `expression` evaluates to in `scope`.
pub(super) fn expression_key<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    expression: &Expr,
) -> AddonKey {
    match expression {
        Expr::StringLiteral(string) => AddonKey::Literal(string.value.to_str().to_string()),
        Expr::Name(name) => name_key(db, scope, name.id.as_str()),
        // `module.NAME`, with `module` a module.
        Expr::Attribute(attribute) => match resolve_expression(db, scope, &attribute.value) {
            Some(Resolved::Module(module)) => {
                name_key(db, global_scope(db, module), attribute.attr.as_str())
            }
            _ => AddonKey::Unknown,
        },
        Expr::Subscript(subscript) => match dot_split_base(subscript) {
            // `e.rpartition(".")[0]`, `e.rsplit(".", 1)[0]`: the package of the module `e` names.
            Some((split @ (DotSplit::Last | DotSplit::LastOrWhole), base)) => {
                match expression_key(db, scope, base) {
                    AddonKey::Module { addon, mut path } if !path.is_empty() => {
                        path.pop();
                        AddonKey::Module { addon, path }
                    }
                    // A legacy add-on's root module name has no dot, so `rsplit` keeps it whole:
                    // `__name__.rsplit(".", 1)[0]` in the root module is the add-on's name. An
                    // extension's root (`bl_ext.<repository>.<id>`) gives its repository package.
                    AddonKey::Module { addon, path }
                        if matches!(split, DotSplit::LastOrWhole)
                            && addon.kind != AddonKind::Extension =>
                    {
                        AddonKey::Module { addon, path }
                    }
                    // `"a.b".rpartition(".")[0]` is `"a"`. A name without a dot gives `""`, or
                    // the whole name for `rsplit`.
                    AddonKey::Literal(literal) => AddonKey::Literal(
                        match (literal.rsplit_once('.'), split) {
                            (Some((head, _)), _) => head,
                            (None, DotSplit::LastOrWhole) => &literal,
                            (None, _) => "",
                        }
                        .to_string(),
                    ),
                    _ => AddonKey::Unknown,
                }
            }
            // `e.split(".")[0]`, `e.partition(".")[0]`: the first component of the name `e` gives.
            Some((DotSplit::First, base)) => match expression_key(db, scope, base) {
                AddonKey::Module { addon, .. } if addon.kind == AddonKind::Extension => {
                    AddonKey::Literal(EXTENSION_PACKAGE.to_string())
                }
                AddonKey::Module { addon, .. } => AddonKey::Module {
                    addon,
                    path: Vec::new(),
                },
                AddonKey::Literal(literal) => {
                    AddonKey::Literal(literal.split('.').next().unwrap_or_default().to_string())
                }
                AddonKey::Unknown => AddonKey::Unknown,
            },
            None => AddonKey::Unknown,
        },
        _ => AddonKey::Unknown,
    }
}

/// The package Blender imports every extension under (`bl_ext.<repository>.<id>`).
const EXTENSION_PACKAGE: &str = "bl_ext";

/// Which component of a dotted name a `[0]` of a split on `"."` takes.
enum DotSplit {
    /// `rpartition(".")[0]`: everything before the last component, `""` without a dot.
    Last,
    /// `rsplit(".", 1)[0]`: everything before the last component, the whole name without a dot.
    LastOrWhole,
    /// `split(".")[0]`, `partition(".")[0]`, `rsplit(".")[0]`: the first component.
    First,
}

/// `e` of a `[0]` of `e` split on `"."`, and which part it takes: `e.rpartition(".")[0]`,
/// `e.partition(".")[0]`, or `e.split(".")[0]` or `e.rsplit(".")[0]` with any `maxsplit` that
/// gives a single part (`e.split(sep=".", maxsplit=1)[0]`). A `maxsplit` of 0 keeps the whole name.
fn dot_split_base(subscript: &ast::ExprSubscript) -> Option<(DotSplit, &Expr)> {
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
    let method_name = method.attr.as_str();
    let is_split = matches!(method_name, "split" | "rsplit");
    if !is_split && !matches!(method_name, "partition" | "rpartition") {
        return None;
    }
    // `partition` takes its separator positionally, `split` also as `sep` and `maxsplit`.
    let arguments = &call.arguments;
    let positional = |index: usize| arguments.args.get(index);
    let keyword = |name: &str| {
        arguments
            .keywords
            .iter()
            .find(|keyword| keyword.arg.as_ref().is_some_and(|arg| arg.as_str() == name))
            .map(|keyword| &keyword.value)
    };
    let max_arguments = if is_split { 2 } else { 1 };
    if arguments.args.len() > max_arguments
        || arguments.args.iter().any(Expr::is_starred_expr)
        || (!is_split && !arguments.keywords.is_empty())
        || arguments.keywords.iter().any(|keyword| {
            !keyword
                .arg
                .as_ref()
                .is_some_and(|arg| matches!(arg.as_str(), "sep" | "maxsplit"))
        })
    {
        return None;
    }
    let Some(Expr::StringLiteral(separator)) = positional(0).or_else(|| keyword("sep")) else {
        return None;
    };
    if separator.value.to_str() != "." {
        return None;
    }
    let max_split = match positional(1).or_else(|| keyword("maxsplit")) {
        None => -1,
        Some(expression) => int_literal(expression)?,
    };
    let split = match (method_name, max_split) {
        ("rpartition", _) => DotSplit::Last,
        ("rsplit", 1) => DotSplit::LastOrWhole,
        ("partition", _) | ("split", ..=-1 | 1..) | ("rsplit", ..=-1) => DotSplit::First,
        _ => return None,
    };
    Some((split, &*method.value))
}

/// The value of an integer literal, negated or not (`1`, `-1`).
fn int_literal(expression: &Expr) -> Option<i64> {
    match expression {
        Expr::NumberLiteral(ast::ExprNumberLiteral {
            value: ast::Number::Int(value),
            ..
        }) => value.as_i64(),
        Expr::UnaryOp(ast::ExprUnaryOp {
            op: ast::UnaryOp::USub,
            operand,
            ..
        }) => int_literal(operand).map(|value| -value),
        _ => None,
    }
}

/// The key the name `name` gives in `scope`: its bindings in the nearest enclosing scope that has
/// any, which must all agree. `__name__` and `__package__` with no binding are the module's own.
fn name_key<'db>(db: &'db dyn Db, scope: ScopeId<'db>, name: &str) -> AddonKey {
    let Some(definitions) = nearest_definitions(db, scope, name) else {
        return dunder_key(db, scope.program_file(db), name);
    };
    agreed(
        definitions
            .iter()
            .map(|definition| definition_key(db, *definition)),
    )
    .cloned()
    .unwrap_or(AddonKey::Unknown)
}

/// The key the name that `definition` binds gives (`bl_idname = __package__`). A cycle of names
/// (`A = B`, `B = A`) gives `Unknown`.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| AddonKey::Unknown,
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn definition_key<'db>(db: &'db dyn Db, definition: Definition<'db>) -> AddonKey {
    let module = parsed_module(db, definition.python_file(db)).load(db);
    match definition.kind(db) {
        DefinitionKind::Assignment(assignment) if assignment.target(&module).is_name_expr() => {
            expression_key(db, definition.scope(db), assignment.value(&module))
        }
        DefinitionKind::AnnotatedAssignment(assignment)
            if assignment.target(&module).is_name_expr() =>
        {
            assignment
                .value(&module)
                .map_or(AddonKey::Unknown, |value| {
                    expression_key(db, definition.scope(db), value)
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
