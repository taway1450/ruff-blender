//! The Add-on index: what an add-on's code does to Blender, gathered without inferring it.
//!
//! Some facts hold for a whole add-on rather than for the file that states them:
//! `bpy.types.Scene.my_value = IntProperty()` in `register()` gives `scene.my_value` to every module of
//! the add-on. The index collects such facts once per add-on:
//!
//! 1. the `register()` walk ([`register_walk`]) follows `register()` through first-party code, shared
//!    libraries included, to the classes it registers and the properties it assigns;
//! 2. when the walk gives up (an `auto_load` module that discovers classes at runtime), the
//!    directory walk fills in: `addon_source_files` (in `ty_blender`) lists the add-on's `.py`
//!    files, and [`file_facts`] scans each file syntactically for registrations and Blender
//!    classes, resolved through the definitions of names (the shared follower,
//!    [`resolve_expression`]), never by inferring the file: inferring `register()` would read the
//!    facts it is busy collecting. A constant or wrapper that a registration names
//!    (`bpy.types.Scene.x = COMMON`) is read by the Property evaluator, in a query of its own;
//! 3. [`addon_index`] combines them, keyed on the add-on and the program.
//!
//! The index is read at the use site, keyed on the add-on of the file that asks, and never stored as
//! class members: class member lookups are cached per class and would leak one add-on's facts into
//! every other add-on.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::{Ranged, TextRange};
use ty_blender::{Addon, REGISTRABLE_ROOTS, addon_source_files};
use ty_python_core::{Program, ProgramFile, semantic_index};

use crate::Db;
use crate::types::{StaticClassLiteral, Type, binding_type};

use super::addon_of_file;
use super::evaluate::{Evaluator, PropertyValue};
use super::follow::{Resolved, resolve_expression};
use super::symbols::{bpy_types_bases, is_bpy_types_class};
use super::walk::{Event, register_walk};

/// What the code of one add-on does to Blender.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct AddonIndex<'db> {
    /// Every Registered property, in the order `register()` assigns them. When the walk is
    /// incomplete, the registrations the directory walk found follow, in source order within a
    /// file and in file-walk order across files.
    pub(super) registered_properties: Box<[RegisteredProperty<'db>]>,
    /// The classes `register()` registers, in the order of their first registration.
    pub(super) registered_classes: Box<[StaticClassLiteral<'db>]>,
    /// Whether `register()` was followed to its end. When it was not, an unknown part of the
    /// add-on's classes is registered.
    pub(super) complete: bool,
}

/// What [`file_facts`] finds in one file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct FileFacts<'db> {
    /// The registrations of the file, in source order, wherever they are.
    pub(super) registered_properties: Box<[RegisteredProperty<'db>]>,
    /// The module-level classes of the file that `register_class` accepts.
    pub(super) classes: Box<[FileClass<'db>]>,
}

/// A class that `bpy.utils.register_class` accepts: one whose method resolution order has a
/// `bpy.types` class with register functions (`Operator`, `Panel`, `PropertyGroup`, ...).
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct FileClass<'db> {
    pub(super) class: StaticClassLiteral<'db>,
    /// The class name in the `class` statement.
    pub(super) name_range: TextRange,
}

/// Whether `class` is a class `bpy.utils.register_class` accepts.
pub(super) fn is_registrable<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    !is_bpy_types_class(db, class)
        && bpy_types_bases(db, class).any(|base| REGISTRABLE_ROOTS.contains(&base))
}

/// One Registered property: `owner.name = <declaration>`, or `setattr(owner, "name", <declaration>)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct RegisteredProperty<'db> {
    pub(super) owner: StaticClassLiteral<'db>,
    /// The owner expression (`bpy.types.Scene`), in the file that registers the property.
    pub(super) owner_range: TextRange,
    /// Whether the owner accepts Registered properties. Blender keeps an assignment to any other
    /// class (`bpy.types.Area.x = IntProperty()`) as a plain class attribute and creates no property.
    pub(super) accepted: bool,
    pub(super) name: Name,
    pub(super) value: PropertyValue<'db>,
}

/// The `bpy.types` classes whose subclasses accept Registered properties: the classes Blender gives
/// a metaclass that turns an attribute assignment into a property (`bpy_struct_meta_idprop`).
/// Blender's own `UIList` and `AddonPreferences` have ID properties, but not that metaclass.
///
/// An allowlist rather than a denylist: when a Blender version adds a struct, a missed warning costs
/// less than a false one. A `check:load` probe in the feature tests compares it with Blender.
const ACCEPTING_ROOTS: [&str; 26] = [
    "ID",
    "PropertyGroup",
    "Node",
    "NodeSocket",
    "NodeTreeInterfaceSocket",
    "Bone",
    "EditBone",
    "PoseBone",
    "BoneCollection",
    "ViewLayer",
    "Strip",
    "TimelineMarker",
    "View3DShading",
    "IDPropertyWrapPtr",
    "OperatorProperties",
    "GizmoProperties",
    "GizmoGroupProperties",
    "KeyConfigPreferences",
    "NodesModifierProperties",
    "NodesModifierPropertiesEmpty",
    "SceneCompositorEffectProperties",
    "SceneCompositorEffectPropertiesEmpty",
    "SequencerCompositorEffectProperties",
    "SequencerCompositorEffectPropertiesEmpty",
    "SequencerCompositorModifierProperties",
    "SequencerCompositorModifierPropertiesEmpty",
];

/// Whether the `bpy.types` class `owner` accepts Registered properties.
pub(super) fn accepts_registered_properties<'db>(
    db: &'db dyn Db,
    owner: StaticClassLiteral<'db>,
) -> bool {
    bpy_types_bases(db, owner).any(|base| ACCEPTING_ROOTS.contains(&base))
}

/// The add-on `file` belongs to, with the program, or `None` for a file in no add-on.
pub(super) fn addon_in_program<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Option<AddonInProgram<'db>> {
    let addon = addon_of_file(db, file).as_ref()?;
    Some(AddonInProgram::new(db, addon.clone(), file.program(db)))
}

/// The index of the add-on `file` belongs to, or `None` for a file in no add-on.
pub(super) fn addon_index_of<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Option<&'db AddonIndex<'db>> {
    Some(addon_index(db, addon_in_program(db, file)?))
}

/// The Registered properties of the add-on `file` belongs to; none for a file in no add-on.
pub(super) fn registered_properties<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> &'db [RegisteredProperty<'db>] {
    addon_index_of(db, file).map_or(&[], |index| &index.registered_properties)
}

/// An add-on of a program, as the key of [`addon_index`] and the walks.
#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub(super) struct AddonInProgram<'db> {
    #[returns(ref)]
    pub(super) addon: Addon,
    #[returns(copy)]
    pub(super) program: Program<'db>,
}

impl<'db> AddonInProgram<'db> {
    /// The add-on's files, in file-walk order.
    pub(super) fn files(self, db: &'db dyn Db) -> impl Iterator<Item = ProgramFile<'db>> {
        let program = self.program(db);
        addon_source_files(db, self.addon(db))
            .into_iter()
            .map(move |file| ProgramFile::new(db, file, program))
    }
}

/// The index of an add-on: what its `register()` registers, completed by the facts of its files
/// when the walk gave up.
///
/// Nothing here infers add-on code, but resolving a registration's `type=` infers the class it
/// names, and that inference may read the index again. Such a cycle starts from an empty index.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| AddonIndex::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn addon_index<'db>(db: &'db dyn Db, key: AddonInProgram<'db>) -> AddonIndex<'db> {
    let walk = register_walk(db, key);
    let mut registered_properties: Vec<RegisteredProperty<'db>> = Vec::new();
    let mut registered_classes: Vec<StaticClassLiteral<'db>> = Vec::new();
    for event in &walk.events {
        match event {
            Event::SetProperty { property, .. } => {
                if !registered_properties.contains(property) {
                    registered_properties.push(property.clone());
                }
            }
            Event::RegisterClass(event) => {
                if !registered_classes.contains(&event.class) {
                    registered_classes.push(event.class);
                }
            }
            _ => {}
        }
    }
    let complete = walk.is_complete();
    if !complete {
        for file in key.files(db) {
            for property in &file_facts(db, file).registered_properties {
                if !registered_properties.contains(property) {
                    registered_properties.push(property.clone());
                }
            }
        }
    }
    AddonIndex {
        registered_properties: registered_properties.into_boxed_slice(),
        registered_classes: registered_classes.into_boxed_slice(),
        complete,
    }
}

/// The facts of `file`, in source order.
///
/// A syntactic pass finds the candidates: assignments to an attribute whose value may be a property
/// (a call, a name, a dotted name or a subscript), and statement-level `setattr(owner, "name",
/// value)`, anywhere in the file (function bodies, loops and conditionals included). The owner is
/// then resolved through the definitions of its names ([`resolve_expression`]), and a candidate
/// whose owner is a `bpy.types` class is kept when the Property evaluator reads its value as a
/// property: a `bpy.props` call, or a constant, wrapper or `functools.partial` of one.
///
/// The pass also lists the module-level classes that `register_class` accepts.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(super) fn file_facts<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> FileFacts<'db> {
    // Every file of an add-on is parsed on the first lookup; do not keep the syntax trees of closed
    // files around for that.
    let module = parsed_module(db, file.python_file(db)).load_then_clear_on_drop(db);
    let mut candidates = Candidates::default();
    candidates.visit_body(module.suite());
    let index = semantic_index(db, file);
    let classes = module_classes(module.suite())
        .into_iter()
        .filter_map(|statement| {
            let definition = index.try_definition(statement)?;
            let Type::ClassLiteral(class) = binding_type(db, definition) else {
                return None;
            };
            let class = class
                .as_static()
                .filter(|class| is_registrable(db, *class))?;
            Some(FileClass {
                class,
                name_range: statement.name.range,
            })
        })
        .collect();
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
            let argument_type = |argument: &Expr| match resolve_expression(db, scope, argument) {
                Some(Resolved::Value(ty)) => ty,
                Some(Resolved::Function(definition)) => binding_type(db, definition),
                Some(Resolved::Module(_) | Resolved::Constant(_)) | None => Type::unknown(),
            };
            let value = Evaluator::new(db, scope, &argument_type).evaluate(candidate.value)?;
            Some(RegisteredProperty {
                owner,
                owner_range: candidate.owner.range(),
                accepted: accepts_registered_properties(db, owner),
                name: candidate.name,
                value,
            })
        })
        .collect();
    FileFacts {
        registered_properties,
        classes,
    }
}

/// The class statements of a module body, including those under a module-level `if` or `try`.
/// Classes defined in functions are left out: factories make them, and which of them are
/// registered can't be told.
fn module_classes(body: &[ast::Stmt]) -> Vec<&ast::StmtClassDef> {
    let mut classes = Vec::new();
    for statement in body {
        match statement {
            ast::Stmt::ClassDef(class) => classes.push(class),
            ast::Stmt::If(if_statement) => {
                classes.extend(module_classes(&if_statement.body));
                for clause in &if_statement.elif_else_clauses {
                    classes.extend(module_classes(&clause.body));
                }
            }
            ast::Stmt::Try(try_statement) => {
                classes.extend(module_classes(&try_statement.body));
                for handler in &try_statement.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    classes.extend(module_classes(&handler.body));
                }
                classes.extend(module_classes(&try_statement.orelse));
                classes.extend(module_classes(&try_statement.finalbody));
            }
            _ => {}
        }
    }
    classes
}

/// A possible registration found by syntax alone.
struct Candidate<'a> {
    owner: &'a Expr,
    name: Name,
    value: &'a Expr,
}

/// Whether `value` may evaluate to a property declaration.
fn may_be_property(value: &Expr) -> bool {
    matches!(
        value,
        Expr::Call(_) | Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_)
    )
}

#[derive(Default)]
struct Candidates<'a> {
    found: Vec<Candidate<'a>>,
}

impl<'a> StatementVisitor<'a> for Candidates<'a> {
    fn visit_stmt(&mut self, statement: &'a ast::Stmt) {
        match statement {
            ast::Stmt::Assign(assign) => {
                let value = &*assign.value;
                if may_be_property(value) {
                    self.found
                        .extend(assign.targets.iter().filter_map(|target| {
                            let target = target.as_attribute_expr()?;
                            Some(Candidate {
                                owner: &target.value,
                                name: target.attr.id.clone(),
                                value,
                            })
                        }));
                }
            }
            ast::Stmt::Expr(expression) => {
                if let Expr::Call(setattr) = &*expression.value
                    && let Expr::Name(function) = &*setattr.func
                    && function.id.as_str() == "setattr"
                    && setattr.arguments.keywords.is_empty()
                    && let [owner, Expr::StringLiteral(name), value] = &*setattr.arguments.args
                    && may_be_property(value)
                {
                    self.found.push(Candidate {
                        owner,
                        name: Name::new(name.value.to_str()),
                        value,
                    });
                }
            }
            _ => {}
        }
        walk_stmt(self, statement);
    }
}
