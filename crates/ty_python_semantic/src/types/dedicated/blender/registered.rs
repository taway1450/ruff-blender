//! Registered properties: `bpy.types.Scene.my_value = IntProperty()` anywhere in an add-on.
//!
//! Blender turns the assignment into a property of `Scene`, so `scene.my_value` is an `int` in every
//! module of the add-on. The registrations are collected per add-on by three tracked queries:
//!
//! 1. `addon_source_files` (in `ty_blender`) lists the add-on's `.py` files;
//! 2. [`registered_properties_in_file`] finds the registrations of one file without inferring it;
//! 3. [`addon_registered_properties`] combines them, in file-walk order.
//!
//! They are looked up at the use site, keyed on the add-on of the file that reads the attribute,
//! never added as class members: class member lookups are cached per class and would leak one
//! add-on's properties into every other add-on. The hooks are [`on_load`] (reads, `+=`, `del`, hover),
//! [`on_store`] (writes) and [`completions`].

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

use crate::place::{Definedness, Place, PlaceAndQualifiers};
use crate::types::attribute_write::{
    AttributeWriteRequirement, ClassAttributeWriteMember, ExplicitAttributeWriteRequirement,
    InstanceAttributeWriteMember,
};
use crate::types::definition_resolution::{
    self, ImportAliasResolution, ResolvedDefinition, find_symbol_in_scope,
};
use crate::types::list_members::Member;
use crate::types::{
    ClassBase, IntersectionType, StaticClassLiteral, SubclassOfInner, Type, TypeQualifiers,
    TypeVarBoundOrConstraints, UnionType, binding_type,
};
use crate::{Db, ProgramEnvironment};

use super::addon_of_file;
use super::properties::PropertyDeclaration;
use super::symbols::is_bpy_types_class;

/// One registration: `owner.name = <declaration>`, or `setattr(owner, "name", <declaration>)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct RegisteredProperty<'db> {
    owner: StaticClassLiteral<'db>,
    name: Name,
    declaration: PropertyDeclaration<'db>,
}

/// An add-on of a program, as the key of [`addon_registered_properties`].
#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
struct AddonInProgram<'db> {
    #[returns(ref)]
    addon: Addon,
    #[returns(copy)]
    program: Program<'db>,
}

/// Every registration of the add-on, in the order Blender would run them as far as it is known:
/// source order within a file, then the order of the add-on's file walk across files.
///
/// Nothing here infers add-on code, but resolving a registration's `type=` infers the class it
/// names, and that inference may read a registered property again. Such a cycle starts from no
/// registrations.
#[salsa::tracked(
    returns(deref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
fn addon_registered_properties<'db>(
    db: &'db dyn Db,
    key: AddonInProgram<'db>,
) -> Box<[RegisteredProperty<'db>]> {
    let program = key.program(db);
    addon_source_files(db, key.addon(db))
        .iter()
        .flat_map(|file| registered_properties_in_file(db, ProgramFile::new(db, *file, program)))
        .cloned()
        .collect()
}

/// The registrations in `file`, in source order.
///
/// A syntactic pass finds the candidates: assignments to an attribute whose value is a call, and
/// statement-level `setattr(owner, "name", call)`, anywhere in the file (function bodies, loops and
/// conditionals included). The owner and the callee are then resolved through the definitions of
/// their names ([`resolve_expression`]), never through the inference of the scope they are in:
/// inferring `register()` would read the registered properties it is busy collecting. A candidate is
/// kept when the owner is a `bpy.types` class and the callee is one of the `bpy.props` functions.
#[salsa::tracked(returns(deref), heap_size=ruff_memory_usage::heap_size)]
fn registered_properties_in_file<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Box<[RegisteredProperty<'db>]> {
    // Every file of an add-on is parsed on the first lookup; do not keep the syntax trees of closed
    // files around for that.
    let module = parsed_module(db, file.python_file(db)).load_then_clear_on_drop(db);
    let mut candidates = Candidates::default();
    candidates.visit_body(module.suite());
    if candidates.found.is_empty() {
        return Box::default();
    }

    let index = semantic_index(db, file);
    candidates
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
        .collect()
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

/// The registration that gives instances of `class` the attribute `name` in `file`'s add-on: the
/// most-derived owner in the method resolution order of `class` wins, then the last registration
/// of that owner.
fn registered_property<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<&'db RegisteredProperty<'db>> {
    let registered = registered_in_addon_of(db, file);
    if !registered.iter().any(|property| property.name == name) {
        return None;
    }
    class.iter_mro(db, None).find_map(|base| {
        let ClassBase::Class(base) = base else {
            return None;
        };
        let base = base.class_literal(db).as_static()?;
        registered
            .iter()
            .rev()
            .find(|property| property.owner == base && property.name == name)
    })
}

/// Whether the registration that gives instances of `class` the attribute `name` in `file`'s add-on
/// is read-only: a collection, a pointer to a `PropertyGroup`, or `options={"READ_ONLY"}`.
pub(super) fn is_registered_read_only<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> bool {
    registered_property(db, file, class, name)
        .is_some_and(|property| property.declaration.is_read_only(db))
}

/// The registrations of the add-on `file` belongs to; none for a file in no add-on.
fn registered_in_addon_of<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> &'db [RegisteredProperty<'db>] {
    let Some(addon) = addon_of_file(db, file) else {
        return &[];
    };
    addon_registered_properties(db, AddonInProgram::new(db, addon.clone(), file.program(db)))
}

/// The value type of the attribute `name` read from `receiver` in `file`, when a registration gives
/// it one.
///
/// The receiver is taken apart the way ty's member lookup does: every arm of a union and every
/// constraint of a type variable must have the attribute, from its class or from a registration;
/// a positive element of an intersection is enough; a bounded type variable reads its bound.
fn instance_value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
    name: &str,
) -> Option<Type<'db>> {
    // An arm that ty resolves itself keeps ty's type; the others need a registration.
    let arm_type = |arm: Type<'db>| match arm.member(db, env, name).place {
        Place::Defined(defined) if defined.definedness == Definedness::AlwaysDefined => {
            Some(defined.ty)
        }
        _ => instance_value_type(db, env, file, arm, name),
    };
    let all_arms = |arms: &[Type<'db>]| {
        let types = arms
            .iter()
            .map(|arm| arm_type(*arm))
            .collect::<Option<Vec<_>>>()?;
        Some(UnionType::from_elements(db, env, types))
    };
    match receiver {
        Type::NominalInstance(instance) => {
            let class = instance.class(db, env).class_literal(db).as_static()?;
            let registered = registered_property(db, file, class, name)?;
            Some(registered.declaration.value_type(db, env))
        }
        Type::Union(union) => all_arms(union.elements(db)),
        Type::Intersection(intersection) => {
            let types = intersection
                .positive(db)
                .iter()
                .filter_map(|element| arm_type(*element))
                .collect::<Vec<_>>();
            (!types.is_empty()).then(|| IntersectionType::from_elements(db, env, types))
        }
        Type::TypeVar(typevar) => match typevar.typevar(db).bound_or_constraints(db, env)? {
            TypeVarBoundOrConstraints::UpperBound(bound) => arm_type(bound),
            TypeVarBoundOrConstraints::Constraints(constraints) => {
                all_arms(constraints.elements(db))
            }
        },
        Type::TypeAlias(alias) => instance_value_type(db, env, file, alias.value_type(db), name),
        _ => None,
    }
}

/// The class behind a class-object receiver (`bpy.types.Scene`, `type[Scene]`).
fn class_receiver<'db>(db: &'db dyn Db, receiver: Type<'db>) -> Option<StaticClassLiteral<'db>> {
    match receiver {
        Type::ClassLiteral(class) => class.as_static(),
        Type::SubclassOf(subclass_of) => match subclass_of.subclass_of() {
            SubclassOfInner::Class(class) => class.class_literal(db).as_static(),
            _ => None,
        },
        _ => None,
    }
}

/// The attribute `attribute` read from `receiver` in `file`, where ty found `place`.
///
/// When ty finds nothing, or finds it on only some arms of the receiver, a registration in the
/// file's add-on gives the attribute its value type. On a class object only `del` is accepted
/// (`del bpy.types.Scene.my_value` in `unregister()`): a plain read gives the property declaration
/// at runtime, not the value, and stays unresolved.
pub(crate) fn on_load<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
    place: PlaceAndQualifiers<'db>,
) -> PlaceAndQualifiers<'db> {
    if place.place.is_definitely_bound() {
        return place;
    }
    let name = attribute.attr.id.as_str();
    if let Some(class) = class_receiver(db, receiver) {
        return if attribute.ctx.is_del()
            && place.is_undefined()
            && registered_property(db, file, class, name).is_some()
        {
            Place::bound(Type::unknown()).into()
        } else {
            place
        };
    }
    match instance_value_type(db, env, file, receiver, name) {
        Some(ty) => Place::bound(ty).into(),
        None => place,
    }
}

/// The write requirement of `object_ty.attribute = ...` in `file`, given the one ty computed.
///
/// Where ty finds no declared member, a registration in the file's add-on decides: an instance write
/// must be assignable to the value type, and a write to the class object (the registration itself)
/// is unconstrained.
pub(in crate::types) fn on_store<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &str,
    requirement: AttributeWriteRequirement<'db>,
) -> AttributeWriteRequirement<'db> {
    match requirement {
        AttributeWriteRequirement::Instance {
            object_ty,
            receiver,
            member: InstanceAttributeWriteMember::SetAttr,
        } if let Some(ty) = instance_value_type(db, env, file, object_ty, attribute) => {
            AttributeWriteRequirement::Instance {
                object_ty,
                receiver,
                member: InstanceAttributeWriteMember::Explicit {
                    member: ExplicitAttributeWriteRequirement::AssignableTo {
                        ty,
                        qualifiers: TypeQualifiers::empty(),
                    },
                    fallback: None,
                },
            }
        }
        AttributeWriteRequirement::Class {
            object_ty,
            member: ClassAttributeWriteMember::Unresolved { .. },
        } if class_receiver(db, object_ty)
            .is_some_and(|class| registered_property(db, file, class, attribute).is_some()) =>
        {
            AttributeWriteRequirement::Unconstrained
        }
        requirement => requirement,
    }
}

/// The registered properties that complete `receiver.` in `file`, with their value types.
pub(crate) fn completions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    receiver: Type<'db>,
) -> Vec<Member<'db>> {
    let mut members: Vec<Member<'db>> = Vec::new();
    for property in registered_in_addon_of(db, file) {
        let name = &property.name;
        if members.iter().any(|member| &member.name == name)
            || receiver.member(db, env, name).place.is_definitely_bound()
        {
            continue;
        }
        if let Some(ty) = instance_value_type(db, env, file, receiver, name) {
            members.push(Member {
                name: name.clone(),
                ty,
                is_type_check_only: false,
            });
        }
    }
    members
}
