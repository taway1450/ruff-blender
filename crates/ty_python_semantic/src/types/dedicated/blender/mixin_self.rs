//! Mixin `self`: `self` in a mixin's methods, typed by the classes that use the mixin.
//!
//! A *mixin* is a first-party class with no `bpy.types` class in its method resolution order, such
//! as Blender's own `ConstraintButtonsPanel`. Python alone types `self` in its methods as the bare
//! mixin, so `self.layout` is an unknown attribute:
//!
//! ```python
//! class ConstraintButtonsPanel:
//!     def draw_header(self, context):
//!         self.layout.label(...)
//!
//! class OBJECT_PT_constraints(ConstraintButtonsPanel, Panel): ...
//! ```
//!
//! Its *users* are the module-level classes that derive from it and have a `bpy.types` class. In
//! Blender, `self` in a mixin method is always an instance of one of them, so the upper bound of
//! `Self` there becomes the mixin intersected with the `bpy.types` classes of the users,
//! `ConstraintButtonsPanel & Panel`. It gives only the members every user's Blender class has, so
//! it hides no mistake in a visible user: a `Panel` mixin calling `self.report` (an `Operator`
//! method) still errors. The bound is silent: it states what holds for every user, so no rule
//! reports it.
//!
//! A class made by a call `type("USERPREF_PT_theme_x", (Mixin, ThemePanel, Panel), {...})`
//! anywhere in the scope is a user too, of each base that is a mixin, typed by the `bpy.types`
//! class of its bases: Blender's theme panels are made so.
//!
//! The users are looked up in the mixin's add-on or, for a file in no add-on (Blender's `bl_ui`),
//! in the `.py` files of its folder, through one cached pass per scope. A mixin with no user there
//! keeps the bare type: its users may be in another add-on or made at runtime (`type(...)`).
//!
//! A mixin method also counts as a draw callback (`self.layout` is never `None`) when it is one for
//! every user ([`super::member`]).
//!
//! Test mixins work the same way: a class that derives from `unittest.TestCase` and has no
//! `bpy.types` class is a user too, with `TestCase` in place of the Blender class, so the `self` of
//! a mixin of test cases (`class GenericSliceMixIn` in Blender's tests) is the mixin intersected
//! with `TestCase`, and has the test case's assertions.

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::system::SystemPath;
use ty_blender::folder_source_files;
use ty_module_resolver::file_to_module;
use ty_python_core::{Program, ProgramFile, semantic_index};

use crate::place::global_symbol;
use crate::types::{
    ClassLiteral, IntersectionType, StaticClassLiteral, Type, TypeVarBoundOrConstraints, UnionType,
    binding_type,
};
use crate::{Db, FxIndexMap, ProgramEnvironment};

use super::addon::is_first_party;
use super::classes::mro_classes;
use super::follow::module_by_name;
use super::index::{addon_in_program, file_facts, module_classes};
use super::symbols::is_bpy_types_class;

/// The bounds of `Self` in the methods of `class`: `bounds` (the class itself), or for a mixin with
/// users, the mixin intersected with the `bpy.types` classes of its users.
pub(crate) fn mixin_self_bounds<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: ClassLiteral<'db>,
    bounds: TypeVarBoundOrConstraints<'db>,
) -> TypeVarBoundOrConstraints<'db> {
    let Some(class) = class.as_static() else {
        return bounds;
    };
    let users = mixin_users(db, class);
    if users.is_empty() {
        return bounds;
    }
    let instance =
        |class: StaticClassLiteral<'db>| Type::instance(db, env, class.identity_specialization(db));
    let blender_classes = users
        .iter()
        .filter_map(|user| anchor_class(db, *user).map(instance));
    let blender_classes = UnionType::from_elements(db, env, blender_classes);
    TypeVarBoundOrConstraints::UpperBound(IntersectionType::from_elements(
        db,
        env,
        [instance(class), blender_classes],
    ))
}

/// The users of `class` when it is a mixin: the module-level classes with a `bpy.types` class (or
/// a `unittest.TestCase`, [`anchor_class`]) that derive from it, in its add-on or, for a file in no
/// add-on, in its folder. Empty for any other class.
#[salsa::tracked(
    returns(deref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn mixin_users<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Box<[StaticClassLiteral<'db>]> {
    let file = class.program_file(db);
    // Without `bpy.types` no class has a user; the check keeps every other project from inferring
    // the classes of whole folders.
    if !is_first_party(db, file)
        || module_by_name(db, file, "bpy.types").is_none()
        || anchor_class(db, class).is_some()
    {
        return Box::default();
    }
    let folder = file
        .file(db)
        .path(db)
        .as_system_path()
        .and_then(SystemPath::parent);
    let files: Box<[File]> = match (addon_in_program(db, file), folder) {
        (Some(addon), _) => addon.files(db).map(|file| file.file(db)).collect(),
        (None, Some(folder)) => folder_source_files(db, folder).into(),
        (None, None) => Box::new([file.file(db)]),
    };
    let scope = UserScope::new(db, files, file.program(db));
    scope_mixins(db, scope)
        .iter()
        .find(|(mixin, _)| *mixin == class)
        .map(|(_, users)| users.clone())
        .unwrap_or_default()
}

/// The files that may hold the users of a mixin, as the key of [`scope_mixins`].
#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
struct UserScope<'db> {
    #[returns(deref)]
    files: Box<[File]>,
    #[returns(copy)]
    program: Program<'db>,
}

/// Every mixin with users in `scope`, with its users in file and source order.
///
/// Reading a class's bases may infer code that asks for the `Self` of a mixin again; such a cycle
/// starts from no mixins.
#[salsa::tracked(
    returns(deref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
fn scope_mixins<'db>(
    db: &'db dyn Db,
    scope: UserScope<'db>,
) -> Box<[(StaticClassLiteral<'db>, Box<[StaticClassLiteral<'db>]>)]> {
    let program = scope.program(db);
    let mut mixins: FxIndexMap<StaticClassLiteral<'db>, Vec<StaticClassLiteral<'db>>> =
        FxIndexMap::default();
    for file in scope.files(db) {
        let file = ProgramFile::new(db, *file, program);
        for user in module_class_literals(db, file) {
            if anchor_class(db, *user).is_none() {
                continue;
            }
            for base in mro_classes(db, *user).skip(1) {
                if anchor_class(db, base).is_none() {
                    mixins.entry(base).or_default().push(*user);
                }
            }
        }
        for bases in &file_facts(db, file).type_calls {
            // The made class is not a class literal: its `bpy.types` class stands for it.
            let Some(anchor) = bases.iter().find_map(|base| anchor_class(db, *base)) else {
                continue;
            };
            for base in bases.iter().flat_map(|base| mro_classes(db, *base)) {
                let users = mixins.entry(base).or_default();
                if anchor_class(db, base).is_none() && !users.contains(&anchor) {
                    users.push(anchor);
                }
            }
        }
    }
    mixins.retain(|_, users| !users.is_empty());
    mixins
        .into_iter()
        .map(|(mixin, users)| (mixin, users.into_boxed_slice()))
        .collect()
}

/// The class a user of a mixin is typed by: the `bpy.types` class `class` is the Python half of
/// ([`bpy_types_counterpart`]), else the first `bpy.types` class in the method resolution order of
/// `class`, or without one `unittest.TestCase`. `None` for a class with neither, which may be a
/// mixin.
fn anchor_class<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<StaticClassLiteral<'db>> {
    bpy_types_counterpart(db, class)
        .or_else(|| mro_classes(db, class).find(|base| is_bpy_types_class(db, *base)))
        .or_else(|| mro_classes(db, class).find(|base| is_unittest_test_case(db, *base)))
}

/// The `bpy.types` class that a class of Blender's `_bpy_types` module adds Python methods to.
///
/// `_bpy_types` defines `class PoseBone(_StructRNA, _GenericBone, ...)`, and Blender merges it into
/// `bpy.types.PoseBone`, the class with the bone's RNA properties. Its bases only name `bpy_struct`,
/// so the bound of `Self` in the shared `_GenericBone` methods takes the bone classes from here:
/// `self.head` is the head of a `PoseBone`, `Bone` or `EditBone`.
fn bpy_types_counterpart<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<StaticClassLiteral<'db>> {
    let file = class.program_file(db);
    file_to_module(db, file.resolver_file(db))
        .filter(|module| module.name(db).as_str() == "_bpy_types")?;
    let types = module_by_name(db, file, "bpy.types")?;
    let Type::ClassLiteral(counterpart) = global_symbol(db, types, class.name(db))
        .place
        .ignore_possibly_undefined()?
    else {
        return None;
    };
    counterpart
        .as_static()
        .filter(|counterpart| is_bpy_types_class(db, *counterpart))
}

/// Whether `class` is `unittest.TestCase` (defined in `unittest.case`).
fn is_unittest_test_case<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    class.name(db) == "TestCase"
        && file_to_module(db, class.program_file(db).resolver_file(db))
            .is_some_and(|module| module.name(db).as_str() == "unittest.case")
}

/// The module-level classes of `file`.
#[salsa::tracked(
    returns(deref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
fn module_class_literals<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Box<[StaticClassLiteral<'db>]> {
    let module = parsed_module(db, file.python_file(db)).load_then_clear_on_drop(db);
    let index = semantic_index(db, file);
    module_classes(module.suite())
        .into_iter()
        .filter_map(|statement| {
            let definition = index.try_definition(statement)?;
            let Type::ClassLiteral(class) = binding_type(db, definition) else {
                return None;
            };
            class.as_static()
        })
        .collect()
}
