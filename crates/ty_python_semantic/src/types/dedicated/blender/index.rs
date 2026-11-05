//! The Add-on index: what an add-on's code does to Blender, gathered without inferring it.
//!
//! Some facts hold for a whole add-on rather than for the file that states them:
//! `bpy.types.Scene.my_value = IntProperty()` in `register()` gives `scene.my_value` to every module of
//! the add-on. The index collects such facts once per add-on, in three steps:
//!
//! 1. `addon_source_files` (in `ty_blender`) lists the add-on's `.py` files in walk order;
//! 2. [`file_facts`] scans one file syntactically and resolves what it found through the definitions
//!    of names ([`resolve_expression`]), never through inference: inferring `register()` would read
//!    the facts it is busy collecting;
//! 3. [`addon_index`] combines the files' facts in walk order, keyed on the add-on and the program.
//!
//! The index is read at the use site, keyed on the add-on of the file that asks, and never stored as
//! class members: class member lookups are cached per class and would leak one add-on's facts into
//! every other add-on.
//!
//! Today the index holds the [`RegisteredProperty`]s; registered classes and operators are the next
//! fact kinds.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::{self as ast, Expr};
use ty_blender::{Addon, addon_source_files};
use ty_module_resolver::{ModuleName, file_to_module, resolve_module_confident};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::ScopeId;
use ty_python_core::{Program, ProgramFile, global_scope, place_table, semantic_index};

use crate::types::definition_resolution::{
    self, ImportAliasResolution, ResolvedDefinition, find_symbol_in_scope,
};
use crate::types::{StaticClassLiteral, Type, binding_type};
use crate::{Db, ProgramEnvironment};

use super::addon_of_file;
use super::properties::PropertyDeclaration;
use super::symbols::is_bpy_types_class;

/// What the code of one add-on does to Blender.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct AddonIndex<'db> {
    /// Every Registered property, in the order Blender would run the registrations as far as it is
    /// known: source order within a file, then the order of the add-on's file walk across files.
    pub(super) registered_properties: Box<[RegisteredProperty<'db>]>,
}

/// One Registered property: `owner.name = <declaration>`, or `setattr(owner, "name", <declaration>)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct RegisteredProperty<'db> {
    pub(super) owner: StaticClassLiteral<'db>,
    pub(super) name: Name,
    pub(super) declaration: PropertyDeclaration<'db>,
}

/// The index of the add-on `file` belongs to, or `None` for a file in no add-on.
pub(super) fn addon_index_of<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Option<&'db AddonIndex<'db>> {
    let addon = addon_of_file(db, file).as_ref()?;
    Some(addon_index(
        db,
        AddonInProgram::new(db, addon.clone(), file.program(db)),
    ))
}

/// The Registered properties of the add-on `file` belongs to; none for a file in no add-on.
pub(super) fn registered_properties<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> &'db [RegisteredProperty<'db>] {
    addon_index_of(db, file).map_or(&[], |index| &index.registered_properties)
}

/// An add-on of a program, as the key of [`addon_index`].
#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
struct AddonInProgram<'db> {
    #[returns(ref)]
    addon: Addon,
    #[returns(copy)]
    program: Program<'db>,
}

/// The index of an add-on: the facts of its files, in file-walk order.
///
/// Nothing here infers add-on code, but resolving a registration's `type=` infers the class it
/// names, and that inference may read the index again. Such a cycle starts from an empty index.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| AddonIndex::default(),
    heap_size=ruff_memory_usage::heap_size
)]
fn addon_index<'db>(db: &'db dyn Db, key: AddonInProgram<'db>) -> AddonIndex<'db> {
    let program = key.program(db);
    let registered_properties = addon_source_files(db, key.addon(db))
        .into_iter()
        .flat_map(|file| {
            file_facts(db, ProgramFile::new(db, file, program))
                .registered_properties
                .iter()
                .cloned()
        })
        .collect();
    AddonIndex {
        registered_properties,
    }
}

/// The facts of `file`, in source order.
///
/// A syntactic pass finds the candidates: assignments to an attribute whose value is a call, and
/// statement-level `setattr(owner, "name", call)`, anywhere in the file (function bodies, loops and
/// conditionals included). The owner and the callee are then resolved through the definitions of
/// their names ([`resolve_expression`]). A candidate is kept when the owner is a `bpy.types` class
/// and the callee is one of the `bpy.props` functions.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn file_facts<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> AddonIndex<'db> {
    // Every file of an add-on is parsed on the first lookup; do not keep the syntax trees of closed
    // files around for that.
    let module = parsed_module(db, file.python_file(db)).load_then_clear_on_drop(db);
    let mut candidates = Candidates::default();
    candidates.visit_body(module.suite());
    if candidates.found.is_empty() {
        return AddonIndex::default();
    }

    let index = semantic_index(db, file);
    let registered_properties = candidates
        .found
        .into_iter()
        .filter_map(|candidate| {
            let scope = index
                .try_expression_scope_id(candidate.owner)?
                .to_scope_id(db, file);
            let Some(Resolved::Value(Type::ClassLiteral(owner))) =
                resolve_expression(db, scope, candidate.owner)
            else {
                return None;
            };
            let owner = owner
                .as_static()
                .filter(|owner| is_bpy_types_class(db, *owner))?;
            let Some(Resolved::Value(callee)) = resolve_expression(db, scope, &candidate.call.func)
            else {
                return None;
            };
            let declaration =
                PropertyDeclaration::from_call(db, scope, callee, candidate.call, |argument| {
                    match resolve_expression(db, scope, argument) {
                        Some(Resolved::Value(ty)) => ty,
                        Some(Resolved::Module(_)) | None => Type::unknown(),
                    }
                })?;
            Some(RegisteredProperty {
                owner,
                name: candidate.name,
                declaration,
            })
        })
        .collect();
    AddonIndex {
        registered_properties,
    }
}

/// A possible registration found by syntax alone.
struct Candidate<'a> {
    owner: &'a Expr,
    name: Name,
    call: &'a ast::ExprCall,
}

#[derive(Default)]
struct Candidates<'a> {
    found: Vec<Candidate<'a>>,
}

impl<'a> StatementVisitor<'a> for Candidates<'a> {
    fn visit_stmt(&mut self, statement: &'a ast::Stmt) {
        match statement {
            ast::Stmt::Assign(assign) => {
                if let Expr::Call(call) = &*assign.value {
                    self.found
                        .extend(assign.targets.iter().filter_map(|target| {
                            let target = target.as_attribute_expr()?;
                            Some(Candidate {
                                owner: &target.value,
                                name: target.attr.id.clone(),
                                call,
                            })
                        }));
                }
            }
            ast::Stmt::Expr(expression) => {
                if let Expr::Call(setattr) = &*expression.value
                    && let Expr::Name(function) = &*setattr.func
                    && function.id.as_str() == "setattr"
                    && setattr.arguments.keywords.is_empty()
                    && let [owner, Expr::StringLiteral(name), Expr::Call(call)] =
                        &*setattr.arguments.args
                {
                    self.found.push(Candidate {
                        owner,
                        name: Name::new(name.value.to_str()),
                        call,
                    });
                }
            }
            _ => {}
        }
        walk_stmt(self, statement);
    }
}

/// What a registration's owner, callee or `type=` refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolved<'db> {
    Module(ProgramFile<'db>),
    /// The type of a class or function definition.
    Value(Type<'db>),
}

/// How many assignments (`T = bpy.types`) are followed at most, which also stops alias cycles.
const MAX_ALIAS_DEPTH: usize = 8;

/// Resolve `expression`, evaluated in `scope`, to a module, class or function by following the
/// definitions of its names: imports (including star imports and re-exports through other modules),
/// aliases and module attributes. Only the types of class and function definitions are inferred.
/// `None` when a name has several different definitions or anything else is involved.
fn resolve_expression<'db>(
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
        // `T = bpy.types` or `Group = bpy.types.PropertyGroup`: follow the assigned value.
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
        }
        _ => None,
    }
}
