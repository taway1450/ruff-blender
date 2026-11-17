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
//! The users are looked up in the mixin's add-on or, for a file in no add-on (Blender's `bl_ui`),
//! in the `.py` files of its folder, through one cached pass per scope. A mixin with no user there
//! keeps the bare type: its users may be in another add-on or made at runtime (`type(...)`).
//!
//! A mixin method also counts as a draw callback (`self.layout` is never `None`) when it is one for
//! every user ([`super::member`]).

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::system::SystemPath;
use ty_blender::folder_source_files;
use ty_python_core::{Program, ProgramFile, semantic_index};

use crate::types::{
    ClassLiteral, IntersectionType, StaticClassLiteral, Type, TypeVarBoundOrConstraints, UnionType,
    binding_type,
};
use crate::{Db, FxIndexMap, ProgramEnvironment};

use super::addon::is_first_party;
use super::classes::mro_classes;
use super::follow::module_by_name;
use super::index::{addon_in_program, module_classes};
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
    let blender_classes = users.iter().filter_map(|user| {
        mro_classes(db, *user)
            .find(|base| is_bpy_types_class(db, *base))
            .map(instance)
    });
    let blender_classes = UnionType::from_elements(db, env, blender_classes);
    TypeVarBoundOrConstraints::UpperBound(IntersectionType::from_elements(
        db,
        env,
        [instance(class), blender_classes],
    ))
}

/// The users of `class` when it is a mixin: the module-level classes with a `bpy.types` class that
/// derive from it, in its add-on or, for a file in no add-on, in its folder. Empty for any other
/// class.
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
        || has_bpy_types_class(db, class)
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
        for user in module_class_literals(db, ProgramFile::new(db, *file, program)) {
            if !has_bpy_types_class(db, *user) {
                continue;
            }
            for base in mro_classes(db, *user).skip(1) {
                if !has_bpy_types_class(db, base) {
                    mixins.entry(base).or_default().push(*user);
                }
            }
        }
    }
    mixins
        .into_iter()
        .map(|(mixin, users)| (mixin, users.into_boxed_slice()))
        .collect()
}

/// Whether the method resolution order of `class` has a `bpy.types` class.
fn has_bpy_types_class<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    mro_classes(db, class).any(|base| is_bpy_types_class(db, base))
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
