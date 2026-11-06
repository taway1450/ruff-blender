//! The Add-on index: what an add-on's code does to Blender, gathered without inferring it.
//!
//! Some facts hold for a whole add-on rather than for the file that states them:
//! `bpy.types.Scene.my_value = IntProperty()` in `register()` gives `scene.my_value` to every module of
//! the add-on. The index collects such facts once per add-on, in three steps:
//!
//! 1. `addon_source_files` (in `ty_blender`) lists the add-on's `.py` files in walk order;
//! 2. [`file_facts`] scans one file syntactically and resolves what it found through the definitions
//!    of names (the shared follower, [`resolve_expression`]), never by inferring the file:
//!    inferring `register()` would read the facts it is busy collecting. A constant or wrapper
//!    that a registration names (`bpy.types.Scene.x = COMMON`) is read by the Property evaluator,
//!    in a query of its own;
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
use ruff_text_size::{Ranged, TextRange};
use ty_blender::{Addon, addon_source_files};
use ty_python_core::{Program, ProgramFile, semantic_index};

use crate::Db;
use crate::types::{StaticClassLiteral, Type};

use super::addon_of_file;
use super::evaluate::{Evaluator, PropertyValue};
use super::follow::{Resolved, resolve_expression};
use super::symbols::{bpy_types_bases, is_bpy_types_class};

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
fn accepts_registered_properties<'db>(db: &'db dyn Db, owner: StaticClassLiteral<'db>) -> bool {
    bpy_types_bases(db, owner).any(|base| ACCEPTING_ROOTS.contains(&base))
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
/// A syntactic pass finds the candidates: assignments to an attribute whose value may be a property
/// (a call, a name, a dotted name or a subscript), and statement-level `setattr(owner, "name",
/// value)`, anywhere in the file (function bodies, loops and conditionals included). The owner is
/// then resolved through the definitions of its names ([`resolve_expression`]), and a candidate
/// whose owner is a `bpy.types` class is kept when the Property evaluator reads its value as a
/// property: a `bpy.props` call, or a constant, wrapper or `functools.partial` of one.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(super) fn file_facts<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> AddonIndex<'db> {
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
            let argument_type = |argument: &Expr| match resolve_expression(db, scope, argument) {
                Some(Resolved::Value(ty)) => ty,
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
    AddonIndex {
        registered_properties,
    }
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
