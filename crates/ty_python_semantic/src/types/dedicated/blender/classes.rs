//! Facts about the classes of an add-on that several features read: their method resolution order,
//! their class attributes as Blender reads them, and which of them may be registered.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::Ranged;
use rustc_hash::FxHashSet;
use ty_blender::OperatorIdname;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::{place_table, use_def_map};

use crate::Db;
use crate::types::class_base::ClassBase;
use crate::types::{
    LiteralValueType, StaticClassLiteral, Type, binding_type, definition_expression_type_in_scope,
};

use super::index::{AddonInProgram, addon_index, file_facts};
use super::symbols::{bpy_types_bases, is_bpy_types_class};
use super::walk::Site;

/// Whether `class` is a class of an add-on (not one of `bpy.types`) that derives from one of the
/// `bpy.types` classes `roots`.
pub(super) fn derives_from(db: &dyn Db, class: StaticClassLiteral<'_>, roots: &[&str]) -> bool {
    !is_bpy_types_class(db, class) && bpy_types_bases(db, class).any(|base| roots.contains(&base))
}

/// The classes of an add-on that may be registered: those `register()` registers, in order, and
/// when the walk gave up, every other registrable class of the add-on's files after them.
pub(super) fn maybe_registered_classes<'db>(
    db: &'db dyn Db,
    key: AddonInProgram<'db>,
) -> Vec<StaticClassLiteral<'db>> {
    let index = addon_index(db, key);
    let mut classes: Vec<StaticClassLiteral<'db>> = index.registered_classes.to_vec();
    if !index.complete {
        for file in key.files(db) {
            for class in &file_facts(db, file).classes {
                if !classes.contains(&class.class) {
                    classes.push(class.class);
                }
            }
        }
    }
    classes
}

/// The classes of `classes` that another of them derives from. Blender never needs them
/// registered: a registered subclass copies what it inherits.
pub(super) fn only_bases<'db>(
    db: &'db dyn Db,
    classes: impl IntoIterator<Item = StaticClassLiteral<'db>> + Clone,
) -> FxHashSet<StaticClassLiteral<'db>> {
    let all: FxHashSet<_> = classes.clone().into_iter().collect();
    classes
        .into_iter()
        .flat_map(|class| mro_classes(db, class).skip(1))
        .filter(|base| all.contains(base))
        .collect()
}

/// The static classes of the method resolution order of `class`, starting with `class` itself.
pub(super) fn mro_classes<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> impl Iterator<Item = StaticClassLiteral<'db>> {
    class.iter_mro(db, None).filter_map(move |base| match base {
        ClassBase::Class(class_type) => class_type.class_literal(db).as_static(),
        _ => None,
    })
}

/// The name a definition binds: the `class` or `def` name, or the assignment target.
pub(super) fn definition_site(db: &dyn Db, definition: Definition<'_>) -> Site {
    let module = parsed_module(db, definition.python_file(db)).load(db);
    Site {
        file: definition.file(db),
        range: definition.focus_range(db, &module).range(),
    }
}

/// The string value of the class attribute `name`, when it is a string literal.
pub(super) fn class_string(
    db: &dyn Db,
    class: StaticClassLiteral<'_>,
    name: &str,
) -> Option<String> {
    class_attribute(db, class, name)?.1
}

/// The last binding of the class attribute `name` in the nearest class of the add-on's method
/// resolution order that binds it, with its value when that is a string literal. The `bpy.types`
/// classes only declare it, so they end the search.
pub(super) fn class_attribute<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<(Definition<'db>, Option<String>)> {
    for base in mro_classes(db, class) {
        if is_bpy_types_class(db, base) {
            return None;
        }
        if let Some(binding) = own_binding(db, base, name) {
            return Some(binding);
        }
    }
    None
}

/// The last binding of `name` in the body of `class`, with its value when that is a string
/// literal.
pub(super) fn own_binding<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<(Definition<'db>, Option<String>)> {
    let scope = class.body_scope(db);
    let symbol = place_table(db, scope).symbol_id(name)?;
    let definition = use_def_map(db, scope)
        .end_of_scope_symbol_bindings(symbol)
        .filter_map(|binding| binding.binding.definition())
        .last()?;
    let value = binding_type(db, definition)
        .as_literal_value()
        .and_then(LiteralValueType::as_string)
        .map(|string| string.value(db).to_string())
        .or_else(|| folded_idname(db, definition).clone());
    Some((definition, value))
}

/// The value of `bl_idname = Other.bl_idname + "_copy"`, which ty widens to `str`: the class
/// attribute is declared `bl_idname: str` in `bpy.types`. The other class's idname is read as
/// Blender does, and the string literals after it are appended.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn folded_idname<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Option<String> {
    let DefinitionKind::Assignment(assignment) = definition.kind(db) else {
        return None;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let mut value = assignment.value(&module);
    let mut suffix = String::new();
    while let Expr::BinOp(ast::ExprBinOp {
        left,
        op: ast::Operator::Add,
        right,
        ..
    }) = value
    {
        let Expr::StringLiteral(string) = &**right else {
            return None;
        };
        suffix.insert_str(0, string.value.to_str());
        value = left;
    }
    let Expr::Attribute(attribute) = value else {
        return None;
    };
    if suffix.is_empty() || attribute.attr.as_str() != "bl_idname" {
        return None;
    }
    let owner =
        definition_expression_type_in_scope(db, definition, &attribute.value, definition.scope(db));
    let Type::ClassLiteral(owner) = owner else {
        return None;
    };
    let (_, idname) = class_attribute(db, owner.as_static()?, "bl_idname")?;
    Some(idname? + &suffix)
}

/// The operator idname `class` registers under, when it is one Blender accepts: its `bl_idname`
/// as Blender reads it from the class and the classes it derives from.
pub(super) fn operator_idname(
    db: &dyn Db,
    class: StaticClassLiteral<'_>,
) -> Option<OperatorIdname> {
    let (_, idname) = class_attribute(db, class, "bl_idname")?;
    OperatorIdname::from_bl_idname(&idname?)
}
