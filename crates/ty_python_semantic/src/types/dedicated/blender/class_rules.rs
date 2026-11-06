//! The class-shape rules: what Blender does with a class's bases that its stubs don't show.
//!
//! - `blender-mixin-order`: Blender's own Python base of a `bpy.types` class (`Operator`,
//!   `Menu`, `Node`, ...) defines a few methods (`Operator.as_keywords`, `Menu.append`,
//!   `Node.poll`, the table in `ty_blender::callbacks`). A mixin listed after the Blender base
//!   loses them to it, silently:
//!
//!   ```python
//!   class MY_OT_run(Operator, KeywordsMixin):  # warning: KeywordsMixin.as_keywords is never used
//!       ...
//!   ```
//!
//!   The table is read, not the stubs: they also declare RNA callbacks (`Operator.poll`), which
//!   Blender looks up on the whole class, so a mixin's `poll` is used in either order.
//! - `blender-invalid-base`: a class with two unrelated `bpy.types` bases (`Operator, Panel`).
//!   Blender registers it as the first one. It replaces ty's own inherited-conflict checks on
//!   that class ([`has_invalid_bases`]), so the mistake is reported once.
//! - ty's own `conflicting-metaclass` and `instance-layout-conflict`, for what the stubs may not
//!   declare: every registrable Blender class has a metaclass of Blender's and a C base with its
//!   own layout, so a mixin with another metaclass (`ABC`) or with `__slots__` makes Blender raise
//!   `TypeError` when the class is created. Reported only while ty sees neither on the Blender
//!   base, so a stub that declares them takes over.
//!
//! The rules hold for every class of a first-party file deriving from a `bpy.types` class,
//! registered or not.

use ruff_python_ast as ast;
use ruff_text_size::Ranged;
use ty_blender::python_base_names;

use crate::Db;
use crate::types::class_base::ClassBase;
use crate::types::context::InferContext;
use crate::types::diagnostic::{CONFLICTING_METACLASS, INSTANCE_LAYOUT_CONFLICT};
use crate::types::{ClassType, KnownClass, StaticClassLiteral, Type};

use super::classes::{mro_classes, own_binding};
use super::lints::{BLENDER_INVALID_BASE, BLENDER_MIXIN_ORDER};
use super::symbols::{bpy_types_class_name, is_bpy_types_class};

/// Report the class-shape rules on `class`, whose name is `name`.
pub(super) fn check_class_rules<'db>(
    context: &InferContext<'db, '_>,
    class: StaticClassLiteral<'db>,
    name: &ast::Identifier,
) {
    let db = context.db();
    if is_bpy_types_class(db, class) {
        return;
    }
    let Some(blender_base) = mro_classes(db, class).find(|base| is_bpy_types_class(db, *base))
    else {
        return;
    };
    if let Some(other) = other_blender_base(db, class) {
        if let Some(builder) = context.report_lint(&BLENDER_INVALID_BASE, name.range()) {
            builder.into_diagnostic(format_args!(
                "`{}` derives from both `{}` and `{}`; Blender registers it as a `{}` only",
                class.name(db),
                blender_base.name(db),
                other.name(db),
                blender_base.name(db),
            ));
        }
    } else if let Some(lost) = lost_mixin_name(db, class) {
        if let Some(builder) = context.report_lint(&BLENDER_MIXIN_ORDER, name.range()) {
            builder.into_diagnostic(format_args!(
                "`{mixin}.{name}` is never used: Blender's `{base}` defines `{name}` and comes first; list `{mixin}` before `{base}`",
                mixin = lost.mixin.name(db),
                name = lost.name,
                base = lost.base,
            ));
        }
    }
    check_metaclass_and_layout(context, class, blender_base, name);
}

/// Whether `class` has two unrelated `bpy.types` bases (`class MY_OT_run(Operator, Panel)`).
/// ty's own checks of the conflicts between its bases stay silent on it, since
/// `blender-invalid-base` reports it.
pub(crate) fn has_invalid_bases(db: &dyn Db, class: StaticClassLiteral<'_>) -> bool {
    !is_bpy_types_class(db, class) && other_blender_base(db, class).is_some()
}

/// The first `bpy.types` class in the method resolution order of `class` that is not in the
/// method resolution order of the first one.
fn other_blender_base<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<StaticClassLiteral<'db>> {
    let mut blender = mro_classes(db, class).filter(|base| is_bpy_types_class(db, *base));
    let first = blender.next()?;
    let first_mro: Vec<_> = mro_classes(db, first).collect();
    blender.find(|base| !first_mro.contains(base))
}

/// A name of Blender's Python base `base` that hides the same name in `mixin`.
struct LostName<'db> {
    base: &'db str,
    mixin: StaticClassLiteral<'db>,
    name: &'static str,
}

/// The first Python base name that a `bpy.types` class in the method resolution order of `class`
/// binds first, while a later class that is not one of `bpy.types` binds it too.
fn lost_mixin_name<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> Option<LostName<'db>> {
    let mro: Vec<_> = mro_classes(db, class).collect();
    // Whether `mro[index]` binds `name` at runtime: a `bpy.types` class by its Python base, any
    // other class by its body. `object` defines the dunders Blender's bases override.
    let binds = |index: usize, name: &str| {
        let base = mro[index];
        match bpy_types_class_name(db, base) {
            Some(base) => python_base_names(base).contains(&name),
            None => !base.is_known(db, KnownClass::Object) && own_binding(db, base, name).is_some(),
        }
    };
    for (index, base) in mro.iter().enumerate() {
        let Some(base_name) = bpy_types_class_name(db, *base) else {
            continue;
        };
        for name in python_base_names(base_name) {
            if (0..index).any(|earlier| binds(earlier, name)) {
                continue;
            }
            if let Some(mixin) = (index + 1..mro.len())
                .find(|&later| !is_bpy_types_class(db, mro[later]) && binds(later, name))
            {
                return Some(LostName {
                    base: base_name,
                    mixin: mro[mixin],
                    name,
                });
            }
        }
    }
    None
}

/// Report ty's own `conflicting-metaclass` and `instance-layout-conflict` for a base of `class`
/// that is not one of Blender's and has a metaclass other than `type`, or a layout of its own,
/// while ty sees neither on `blender_base`, the first `bpy.types` class in its method resolution
/// order.
fn check_metaclass_and_layout<'db>(
    context: &InferContext<'db, '_>,
    class: StaticClassLiteral<'db>,
    blender_base: StaticClassLiteral<'db>,
    name: &ast::Identifier,
) {
    let db = context.db();
    let is_type = |metaclass: Type<'db>| {
        metaclass
            .to_class_type(db)
            .is_some_and(|metaclass| metaclass.is_known(db, KnownClass::Type))
    };
    let mixins: Vec<ClassType<'db>> = class
        .iter_mro(db, None)
        .skip(1)
        .filter_map(|base| match base {
            ClassBase::Class(base) => Some(base),
            _ => None,
        })
        .filter(|base| {
            base.class_literal(db)
                .as_static()
                .is_some_and(|base| !mro_classes(db, base).any(|c| is_bpy_types_class(db, c)))
        })
        .collect();
    let blender_name = blender_base.name(db);
    if is_type(blender_base.metaclass(db))
        && let Some(mixin) = mixins.iter().find(|mixin| {
            let metaclass = mixin.metaclass(db);
            !metaclass.is_dynamic() && !is_type(metaclass)
        })
        && let Some(builder) = context.report_lint(&CONFLICTING_METACLASS, name.range())
    {
        builder.into_diagnostic(format_args!(
            "The metaclass of base class `{}` conflicts with Blender's metaclass of `{blender_name}`",
            mixin.name(db),
        ));
    }
    // Every class has `object`'s layout, which typeshed marks as a disjoint base.
    let has_own_layout = |class: ClassType<'db>| {
        class
            .nearest_disjoint_base(db)
            .is_some_and(|base| !base.class.is_known(db, KnownClass::Object))
    };
    if !has_own_layout(blender_base.default_specialization(db))
        && let Some(mixin) = mixins.iter().find(|mixin| has_own_layout(**mixin))
        && let Some(builder) = context.report_lint(&INSTANCE_LAYOUT_CONFLICT, name.range())
    {
        builder.into_diagnostic(format_args!(
            "Base class `{}` has an instance layout of its own, which conflicts with Blender's C layout of `{blender_name}`",
            mixin.name(db),
        ));
    }
}
