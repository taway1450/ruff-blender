//! Add-on preferences: `preferences.addons[KEY].preferences` is the add-on's own registered
//! `AddonPreferences` class when `KEY` names the add-on.
//!
//! Blender finds an add-on's preferences by its module name: `Addon.preferences` is the registered
//! `AddonPreferences` type whose `bl_idname` equals the `Addon`'s `module`, or `None`. A class
//! registered later with the same idname replaces the earlier one. So the class is the last one
//! `register()` registers whose `bl_idname` names the add-on, and the key and the idname are both
//! read by the Add-on key evaluator ([`super::addon_key`]).
//!
//! The result is not a specialised `Addon` type: only the attribute load `.preferences` changes,
//! decided from the receiver's syntax (`<x>.addons[KEY]`, `<x>.addons.get(KEY)`, or a name bound to
//! either). An `Addon` passed into a function has no key there, and keeps the stub type.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::Ranged;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, semantic_index};

use crate::types::{StaticClassLiteral, Type, UnionType};
use crate::{Db, ProgramEnvironment};

use super::addon_key::{AddonKey, definition_key, expression_key};
use super::classes::{
    class_attribute, definition_site, derives_from, maybe_registered_classes, only_bases,
};
use super::follow::{agreed, nearest_definitions};
use super::index::{AddonInProgram, addon_in_program, addon_index};
use super::registration_checks::{Finding, Rule};
use super::symbols::bpy_types_class_name;
use super::walk::Site;

/// The preferences classes of an add-on, and the problems of its preferences classes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct AddonPreferences<'db> {
    /// The classes `addons[<this add-on>].preferences` may be: the last registered match, or every
    /// match when the walk gave up.
    pub(super) classes: Box<[StaticClassLiteral<'db>]>,
    /// The problems for `blender-preferences-idname`.
    pub(super) problems: Box<[Finding]>,
}

/// Whether `class` is an add-on's preferences class.
fn is_preferences_class<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    derives_from(db, class, &["AddonPreferences"])
}

/// The preferences classes of an add-on and the problems of its preferences classes.
///
/// The candidates are the preferences classes `register()` registers, in order; when the walk gave
/// up, every preferences class of the add-on's files that no other one derives from, and any of
/// those that names the add-on may be its preferences. A class whose `bl_idname` does not name the
/// add-on, or has none, is never found by Blender. A later class that names it replaces the earlier
/// one, which then fails to unregister; that needs the order, so it is reported only when the walk
/// is complete. An idname the evaluator can't read is not checked.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| AddonPreferences::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn addon_preferences<'db>(
    db: &'db dyn Db,
    key: AddonInProgram<'db>,
) -> AddonPreferences<'db> {
    let index = addon_index(db, key);
    let mut candidates: Vec<StaticClassLiteral<'db>> = maybe_registered_classes(db, key)
        .into_iter()
        .filter(|class| is_preferences_class(db, *class))
        .collect();
    if !index.complete {
        let bases = only_bases(db, candidates.iter().copied());
        candidates
            .retain(|class| !bases.contains(class) || index.registered_classes.contains(class));
    }
    let addon = key.addon(db);
    let mut chosen: Vec<StaticClassLiteral<'db>> = Vec::new();
    let mut problems = Vec::new();
    let mut report = |site: Site, message: String| {
        problems.push(Finding {
            rule: Rule::PreferencesIdname,
            file: site.file,
            range: site.range,
            message,
        });
    };
    for class in candidates {
        let Some((definition, _)) = class_attribute(db, class, "bl_idname") else {
            report(
                definition_site(db, class.definition(db)),
                format!(
                    "`{}` has no `bl_idname`, so it is never the add-on's preferences: Blender finds them by the add-on's module name",
                    class.name(db)
                ),
            );
            continue;
        };
        let idname = definition_key(db, definition);
        if idname.names(addon) {
            if index.complete {
                if let Some(earlier) = chosen.pop() {
                    report(
                        value_site(db, definition),
                        format!(
                            "`{}` replaces `{}` as the add-on's preferences: Blender keeps the last class registered with this `bl_idname`, and unregistering `{}` then fails",
                            class.name(db),
                            earlier.name(db),
                            earlier.name(db),
                        ),
                    );
                }
            }
            chosen.push(class);
        } else if *idname != AddonKey::Unknown {
            report(
                value_site(db, definition),
                format!(
                    "The `bl_idname` of `{}` does not name this add-on, so it is never the add-on's preferences: use the root package's `__package__`",
                    class.name(db)
                ),
            );
        }
    }
    AddonPreferences {
        classes: chosen.into_boxed_slice(),
        problems: problems.into_boxed_slice(),
    }
}

/// The value of an assignment's definition, or else its target.
fn value_site(db: &dyn Db, definition: Definition<'_>) -> Site {
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let value = match definition.kind(db) {
        DefinitionKind::Assignment(assignment) => Some(assignment.value(&module)),
        DefinitionKind::AnnotatedAssignment(assignment) => assignment.value(&module),
        _ => None,
    };
    match value {
        Some(value) => Site {
            file: definition.file(db),
            range: value.range(),
        },
        None => definition_site(db, definition),
    }
}

/// The type of `attribute` (`<entry>.preferences`) when its receiver is an add-on entry whose key
/// names the add-on of `file`, and the add-on has a preferences class: an instance of that class, or
/// of any of them when the add-on may register several.
pub(super) fn preferences_load<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
) -> Option<Type<'db>> {
    let key = addon_in_program(db, file)?;
    let Type::NominalInstance(instance) = receiver else {
        return None;
    };
    let class = instance.class(db, env).class_literal(db).as_static()?;
    if bpy_types_class_name(db, class) != Some("Addon") {
        return None;
    }
    let index = semantic_index(db, file);
    let scope = index
        .try_expression_scope_id(&*attribute.value)?
        .to_scope_id(db, file);
    if !entry_key(db, scope, &attribute.value).names(key.addon(db)) {
        return None;
    }
    let classes = &addon_preferences(db, key).classes;
    if classes.is_empty() {
        return None;
    }
    Some(UnionType::from_elements(
        db,
        env,
        classes
            .iter()
            .map(|class| Type::instance(db, env, class.default_specialization(db))),
    ))
}

/// The key of an add-on entry: `<x>.addons[KEY]`, `<x>.addons.get(KEY)`, or a name every binding of
/// which is one of these with the same key. `Unknown` for anything else.
fn entry_key<'db>(db: &'db dyn Db, scope: ScopeId<'db>, expression: &Expr) -> AddonKey {
    match expression {
        Expr::Subscript(subscript) if is_addons(&subscript.value) => {
            expression_key(db, scope, &subscript.slice)
        }
        Expr::Call(call) => {
            let Expr::Attribute(method) = &*call.func else {
                return AddonKey::Unknown;
            };
            match &*call.arguments.args {
                [key, ..] if method.attr.as_str() == "get" && is_addons(&method.value) => {
                    expression_key(db, scope, key)
                }
                _ => AddonKey::Unknown,
            }
        }
        Expr::Name(name) => nearest_definitions(db, scope, name.id.as_str())
            .and_then(|definitions| {
                agreed(
                    definitions
                        .iter()
                        .map(|definition| bound_entry_key(db, *definition)),
                )
            })
            .unwrap_or(AddonKey::Unknown),
        _ => AddonKey::Unknown,
    }
}

/// The key of the add-on entry that the binding `definition` assigns.
fn bound_entry_key<'db>(db: &'db dyn Db, definition: Definition<'db>) -> AddonKey {
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let value = match definition.kind(db) {
        DefinitionKind::Assignment(assignment) if assignment.target(&module).is_name_expr() => {
            assignment.value(&module)
        }
        DefinitionKind::NamedExpression(named) => &*named.node(&module).value,
        _ => return AddonKey::Unknown,
    };
    // A name bound to a name is not followed: `entry_key` would loop on `a = a`.
    if value.is_name_expr() {
        return AddonKey::Unknown;
    }
    entry_key(db, definition.scope(db), value)
}

/// Whether `expression` is `<x>.addons`. Only `Preferences.addons` holds `Addon`s, and the receiver
/// of `.preferences` must be one.
fn is_addons(expression: &Expr) -> bool {
    matches!(expression, Expr::Attribute(attribute) if attribute.attr.as_str() == "addons")
}
