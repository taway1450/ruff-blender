//! The shared follower: what a name or dotted name refers to, found through the definitions of its
//! names rather than through inference.
//!
//! The Add-on index uses it to find the owner and the callee of a registration without inferring
//! `register()`, and the Property evaluator uses it to find the constant an annotation names
//! (`x: COMMON`, `x: props.COMMON`).

use indexmap::IndexSet;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_python_ast::{self as ast, Expr};
use ty_module_resolver::{ModuleName, file_to_module, resolve_module_confident};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, global_scope, place_table, semantic_index, use_def_map};

use crate::types::definition_resolution::{
    self, ImportAliasResolution, ResolvedDefinition, find_symbol_in_scope,
};
use crate::types::infer::nearest_enclosing_class;
use crate::types::{StaticClassLiteral, Type, binding_type};
use crate::{Db, ProgramEnvironment};

/// What a name or dotted name refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resolved<'db> {
    Module(ProgramFile<'db>),
    /// The type of a class definition.
    Value(Type<'db>),
    /// A function definition, whose type is its `binding_type`. The `register()` walk follows the
    /// definition into the function's body.
    Function(Definition<'db>),
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

/// The type of `argument`, a part of a registration's value in `scope` (of the file `module`), as
/// the Property evaluator reads it without inferring the registration: what [`resolve_expression`]
/// resolves it to, or the class itself for the `cls` of a classmethod. Blender calls a class's
/// `register()` classmethod when it registers the class, where Cycles assigns its settings:
///
/// ```python
/// class CyclesRenderSettings(bpy.types.PropertyGroup):
///     @classmethod
///     def register(cls):
///         bpy.types.Scene.cycles = PointerProperty(type=cls)
/// ```
pub(super) fn registration_argument_type<'db>(
    db: &'db dyn Db,
    module: &ParsedModuleRef,
    scope: ScopeId<'db>,
    argument: &Expr,
) -> Type<'db> {
    match resolve_expression(db, scope, argument) {
        Some(Resolved::Value(ty)) => ty,
        Some(Resolved::Function(definition)) => binding_type(db, definition),
        Some(Resolved::Module(_) | Resolved::Constant(_)) => Type::unknown(),
        None => {
            classmethod_class(db, module, scope, argument).map_or_else(Type::unknown, Type::from)
        }
    }
}

/// Whether `function` is decorated with `@classmethod`, by the decorator's name.
///
/// The registration walk runs before inference and must not infer anything in the add-on, so it
/// cannot ask ty what the decorator evaluates to, as the override checks do: a `classmethod`
/// rebound in the module is taken for the builtin.
pub(super) fn is_classmethod(function: &ast::StmtFunctionDef) -> bool {
    function.decorator_list.iter().any(|decorator| {
        decorator
            .expression
            .as_name_expr()
            .is_some_and(|decorator| decorator.id.as_str() == "classmethod")
    })
}

/// The class whose body defines the classmethod `scope`, when `argument` is the name of its first
/// parameter.
fn classmethod_class<'db>(
    db: &'db dyn Db,
    module: &ParsedModuleRef,
    scope: ScopeId<'db>,
    argument: &Expr,
) -> Option<StaticClassLiteral<'db>> {
    let Expr::Name(name) = argument else {
        return None;
    };
    let function = scope.node(db).as_function()?.node(module);
    let parameters = &function.parameters;
    let first = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .next()?;
    if !is_classmethod(function) || first.parameter.name.id != name.id {
        return None;
    }
    let index = semantic_index(db, scope.program_file(db));
    let in_class_body = index
        .parent_scope_id(scope.file_scope_id(db))
        .is_some_and(|parent| index.scope(parent).node().as_class().is_some());
    if !in_class_body {
        return None;
    }
    nearest_enclosing_class(db, index, scope)
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

/// Resolve the name `name`, evaluated in `scope`, as [`resolve_expression`] resolves a name.
pub(super) fn resolve_name_in<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    name: &str,
) -> Option<Resolved<'db>> {
    resolve_name(db, scope, name, 0)
}

/// The definitions that the binding `definition` refers to through imports and re-exports: itself
/// for any other binding.
pub(super) fn imported_definitions<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    name: &str,
) -> Vec<ResolvedDefinition<'db>> {
    let env = ProgramEnvironment::from_file(definition.program_file(db));
    definition_resolution::resolve_definition(
        db,
        &env,
        definition,
        Some(name),
        ImportAliasResolution::ResolveAliases,
    )
}

/// The module called `name`, as `importing_file` would import it.
pub(super) fn module_by_name<'db>(
    db: &'db dyn Db,
    importing_file: ProgramFile<'db>,
    name: &str,
) -> Option<ProgramFile<'db>> {
    match resolve_module_file(db, importing_file, &ModuleName::new(name)?)? {
        Resolved::Module(module) => Some(module),
        _ => None,
    }
}

/// The definitions of `name` read in `scope`: those of the nearest enclosing scope that has any.
///
/// A name of an enclosing scope is read when the function runs, after that scope has run: in
/// `register()`, a module-level name is what the module last bound it to. So only the definitions
/// that reach the end of the enclosing scope count there, and `class Text(PropertyGroup)` after
/// `from bpy.types import Text` is the one `Text` in `register()`.
///
/// This assumes the function runs after the module body: a helper called while the module is
/// still importing, before a later rebinding, would read the earlier value instead.
pub(super) fn nearest_definitions<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    name: &str,
) -> Option<IndexSet<Definition<'db>>> {
    let file = scope.program_file(db);
    let index = semantic_index(db, file);
    let own_scope = scope.file_scope_id(db);
    index
        .visible_ancestor_scopes(own_scope)
        .map(|(ancestor, _)| {
            let ancestor_scope = ancestor.to_scope_id(db, file);
            if ancestor == own_scope {
                find_symbol_in_scope(db, ancestor_scope, name)
            } else {
                definitions_at_end(db, ancestor_scope, name)
            }
        })
        .find(|definitions| !definitions.is_empty())
}

/// The definitions of `name` in `scope` that reach its end, or all of them when none of those is
/// one that [`find_symbol_in_scope`] lists.
fn definitions_at_end<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    name: &str,
) -> IndexSet<Definition<'db>> {
    let definitions = find_symbol_in_scope(db, scope, name);
    let at_end = end_of_scope_definitions(db, scope, name);
    let reaching: IndexSet<Definition<'db>> = definitions
        .iter()
        .copied()
        .filter(|definition| at_end.contains(definition))
        .collect();
    if reaching.is_empty() {
        definitions
    } else {
        reaching
    }
}

/// Resolve the module-level name `name` of `file` as a function of the module reads it once the
/// module has run (`register()`): through the definitions that reach the end of the module.
pub(super) fn resolve_module_name_at_end<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    name: &str,
) -> Option<Resolved<'db>> {
    all_same(
        definitions_at_end(db, global_scope(db, file), name)
            .iter()
            .map(|definition| resolve_binding(db, *definition, name, 0)),
    )
}

/// The bindings and declarations of `name` that reach the end of `scope`.
fn end_of_scope_definitions<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    name: &str,
) -> Vec<Definition<'db>> {
    let Some(symbol) = place_table(db, scope).symbol_id(name) else {
        return Vec::new();
    };
    let use_def = use_def_map(db, scope);
    use_def
        .end_of_scope_symbol_bindings(symbol)
        .filter_map(|binding| binding.binding.definition())
        .chain(
            use_def
                .end_of_scope_symbol_declarations(symbol)
                .filter_map(|declaration| declaration.declaration.definition()),
        )
        .collect()
}

/// The value all of `values` agree on, or `None` when there are none or two differ.
pub(super) fn agreed<T: PartialEq>(mut values: impl Iterator<Item = T>) -> Option<T> {
    let first = values.next()?;
    values.all(|value| value == first).then_some(first)
}

/// The name `name` in `scope`: its definitions in the nearest enclosing scope that has any, which
/// must all resolve to the same thing.
fn resolve_name<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    name: &str,
    depth: usize,
) -> Option<Resolved<'db>> {
    all_same(
        nearest_definitions(db, scope, name)?
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
fn all_same<'db>(items: impl Iterator<Item = Option<Resolved<'db>>>) -> Option<Resolved<'db>> {
    agreed(items)?
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
        DefinitionKind::Class(_) => Some(Resolved::Value(binding_type(db, definition))),
        DefinitionKind::Function(_) => Some(Resolved::Function(definition)),
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
