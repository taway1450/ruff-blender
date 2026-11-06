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

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, semantic_index};

use crate::types::definition_resolution::find_symbol_in_scope;
use crate::types::{StaticClassLiteral, Type};
use crate::{Db, ProgramEnvironment};

use super::addon_key::{AddonKey, definition_key, expression_key};
use super::index::{AddonInProgram, addon_in_program, addon_index, file_facts};
use super::registration_checks::{class_attribute, definition_site, mro_classes};
use super::symbols::{bpy_types_bases, bpy_types_class_name, is_bpy_types_class};

/// The preferences class of an add-on, and the problems of its preferences classes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct AddonPreferences<'db> {
    /// The class `addons[<this add-on>].preferences` is: the last registered match.
    pub(super) class: Option<StaticClassLiteral<'db>>,
    /// The problems for `blender-preferences-idname`.
    pub(super) problems: Box<[PreferencesProblem]>,
}

/// A preferences class that is never the add-on's preferences, at a place in the add-on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct PreferencesProblem {
    pub(super) file: File,
    pub(super) range: TextRange,
    pub(super) message: String,
}

/// Whether `class` is an add-on's preferences class.
fn is_preferences_class<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    !is_bpy_types_class(db, class)
        && bpy_types_bases(db, class).any(|base| base == "AddonPreferences")
}

/// The preferences class of an add-on and the problems of its preferences classes.
///
/// The candidates are the preferences classes `register()` registers, in order; when the walk gave
/// up, every preferences class of the add-on's files that no other one derives from. A class whose
/// `bl_idname` does not name the add-on, or has none, is never found by Blender. A later class that
/// names it replaces the earlier one, which then fails to unregister; that needs the order, so it
/// is reported only when the walk is complete. An idname the evaluator can't read is not checked.
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
    let mut candidates: Vec<StaticClassLiteral<'db>> = index
        .registered_classes
        .iter()
        .copied()
        .filter(|class| is_preferences_class(db, *class))
        .collect();
    if !index.complete {
        let mut found = Vec::new();
        for file in key.files(db) {
            for class in &file_facts(db, file).classes {
                if is_preferences_class(db, class.class) && !found.contains(&class.class) {
                    found.push(class.class);
                }
            }
        }
        let bases: Vec<_> = found
            .iter()
            .flat_map(|class| mro_classes(db, *class).skip(1))
            .collect();
        for class in found {
            if !bases.contains(&class) && !candidates.contains(&class) {
                candidates.push(class);
            }
        }
    }
    let addon = key.addon(db);
    let mut chosen: Option<StaticClassLiteral<'db>> = None;
    let mut problems = Vec::new();
    for class in candidates {
        let Some((definition, _)) = class_attribute(db, class, "bl_idname") else {
            let site = definition_site(db, class.definition(db));
            problems.push(PreferencesProblem {
                file: site.file,
                range: site.range,
                message: format!(
                    "`{}` has no `bl_idname`, so it is never the add-on's preferences: Blender finds them by the add-on's module name",
                    class.name(db)
                ),
            });
            continue;
        };
        let idname = definition_key(db, definition);
        if idname.names(addon) {
            if let Some(earlier) = chosen
                && index.complete
            {
                let site = value_site(db, definition);
                problems.push(PreferencesProblem {
                    file: site.0,
                    range: site.1,
                    message: format!(
                        "`{}` replaces `{}` as the add-on's preferences: Blender keeps the last class registered with this `bl_idname`, and unregistering `{}` then fails",
                        class.name(db),
                        earlier.name(db),
                        earlier.name(db),
                    ),
                });
            }
            chosen = Some(class);
        } else if idname != AddonKey::Unknown {
            let site = value_site(db, definition);
            problems.push(PreferencesProblem {
                file: site.0,
                range: site.1,
                message: format!(
                    "The `bl_idname` of `{}` does not name this add-on, so it is never the add-on's preferences: use the root package's `__package__`",
                    class.name(db)
                ),
            });
        }
    }
    AddonPreferences {
        class: chosen,
        problems: problems.into_boxed_slice(),
    }
}

/// The value of an assignment's definition, or else its target.
fn value_site(db: &dyn Db, definition: Definition<'_>) -> (File, TextRange) {
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let range = match definition.kind(db) {
        DefinitionKind::Assignment(assignment) => assignment.value(&module).range(),
        DefinitionKind::AnnotatedAssignment(assignment) => assignment
            .value(&module)
            .map_or_else(|| definition_site(db, definition).range, Ranged::range),
        _ => definition_site(db, definition).range,
    };
    (definition.file(db), range)
}

/// The type of `attribute` (`<entry>.preferences`) when its receiver is an add-on entry whose key
/// names the add-on of `file`, and the add-on has a preferences class: an instance of that class.
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
    let class = addon_preferences(db, key).class?;
    Some(Type::instance(db, env, class.default_specialization(db)))
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
        Expr::Name(name) => {
            let file = scope.program_file(db);
            let index = semantic_index(db, file);
            let Some(definitions) = index
                .visible_ancestor_scopes(scope.file_scope_id(db))
                .map(|(ancestor, _)| {
                    find_symbol_in_scope(db, ancestor.to_scope_id(db, file), name.id.as_str())
                })
                .find(|definitions| !definitions.is_empty())
            else {
                return AddonKey::Unknown;
            };
            let mut keys = definitions
                .iter()
                .map(|definition| bound_entry_key(db, *definition));
            let Some(first) = keys.next() else {
                return AddonKey::Unknown;
            };
            if keys.all(|key| key == first) {
                first
            } else {
                AddonKey::Unknown
            }
        }
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
