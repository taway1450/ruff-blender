//! The Blender lints, reported by one query per file that `check_types` merges before it checks
//! suppressions. That keeps `# ty: ignore[...]` working for them, and keeps a manifest edit from
//! re-inferring the module.
//!
//! - The add-on lints, `blender-invalid-register` and `blender-invalid-addon-metadata`, describe the
//!   add-on as a whole, not one expression, and are reported on the add-on's root module.
//! - The property rules (`blender-invalid-property`, `blender-invalid-operator-property` and the three
//!   warnings about properties Blender ignores) are reported on every first-party file, by [`super::rules`].
//! - The registration checks (`blender-unregistered-class` and five more) are reported on the file of the
//!   add-on they are in, by [`super::registration_checks`].
//! - `blender-unknown-operator` and the `blender-invalid-operator-property` problems of an operator
//!   idname's use are reported while a call is inferred, and for `bl_keymap` tuples here, by
//!   [`super::operator_properties`].
//! - `blender-preferences-idname` is reported on the file of the add-on's preferences class, by
//!   [`super::preferences`].
//! - `blender-invalid-override`, a Blender callback that Blender refuses, is reported on every
//!   first-party file, by [`super::overrides`].
//! - The Imprecise annotation rules (`blender-imprecise-*`, the category
//!   `blender-imprecise-annotations`, [`super::categories`]) are reported by [`super::imprecise`]:
//!   an operator's result annotation and a callback's parameter annotations here, a returned
//!   `set[str]` and an enum value where they are inferred.
//! - The Implied narrowing rules (`blender-narrowed-by-*`, the category
//!   `blender-implied-narrowings`) are reported at the narrowed read, while it is inferred: by
//!   [`super::poll_checks`] for a pointer a `poll()` checked, by [`super::context_members`] for a
//!   menu's idname prefix and a popover panel.
//! - The Assumption rules (`blender-assumed-*`, the category `blender-assumptions`): the context
//!   members by [`super::context_members`] and the uses of a class or property registered on a
//!   branch and of the add-on's preferences by [`super::assumptions`], at the read while it is
//!   inferred; the reads of a list item and where the `register()` walk gave up here.
//! - The class-shape rules, `blender-mixin-order` and `blender-invalid-base`, are reported on every
//!   first-party file, by [`super::class_rules`], from the visitor of the property rules.

use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_blender::{Addon, AddonKind, bl_info, manifest};
use ty_python_core::{ProgramFile, global_scope, place_table, use_def_map};

use crate::lint::{Level, LintRegistryBuilder, LintStatus};
use crate::place::global_symbol;
use crate::types::call::CallArguments;
use crate::types::context::InferContext;
use crate::types::diagnostic::TypeCheckDiagnostics;
use crate::{Db, ProgramEnvironment, declare_lint};

use super::addon::is_first_party;
use super::assumptions::check_assumptions;
use super::addon_of_file;
use super::index::addon_in_program;
use super::operator_properties::check_keymaps;
use super::overrides::check_overrides;
use super::preferences::addon_preferences;
use super::registration_checks::{Finding, Rule, registration_findings};
use super::rules::check_properties;

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-invalid-register.md")]
    pub(crate) static BLENDER_INVALID_REGISTER = {
        summary: "detects a Blender add-on whose `register` or `unregister` is missing or takes arguments",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-invalid-addon-metadata.md")]
    pub(crate) static BLENDER_INVALID_ADDON_METADATA = {
        summary: "detects malformed `bl_info` or `blender_manifest.toml` in a Blender add-on",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-invalid-property.md")]
    pub(crate) static BLENDER_INVALID_PROPERTY = {
        summary: "detects a Blender property declaration that Blender rejects when it registers the property",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-invalid-operator-property.md")]
    pub(crate) static BLENDER_INVALID_OPERATOR_PROPERTY = {
        summary: "detects an operator property that Blender refuses, which drops it and every property after it",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-unsupported-property-host.md")]
    pub(crate) static BLENDER_UNSUPPORTED_PROPERTY_HOST = {
        summary: "detects a Blender property annotation on a class whose annotations Blender ignores",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-legacy-property.md")]
    pub(crate) static BLENDER_LEGACY_PROPERTY = {
        summary: "detects a Blender property assigned in a class body instead of annotated",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-unsupported-property-target.md")]
    pub(crate) static BLENDER_UNSUPPORTED_PROPERTY_TARGET = {
        summary: "detects a Blender property assigned to a class that does not accept Registered properties",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-register-order.md")]
    pub(crate) static BLENDER_REGISTER_ORDER = {
        summary: "detects a Blender class or property registered before the class it points to",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-duplicate-registration.md")]
    pub(crate) static BLENDER_DUPLICATE_REGISTRATION = {
        summary: "detects a Blender class registered twice, or two Blender classes registered with one idname",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-invalid-idname.md")]
    pub(crate) static BLENDER_INVALID_IDNAME = {
        summary: "detects a `bl_idname` that Blender refuses or warns about when it registers the class",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-unregistered-class.md")]
    pub(crate) static BLENDER_UNREGISTERED_CLASS = {
        summary: "detects a Blender class of an add-on that `register()` never registers",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-unreachable-registration.md")]
    pub(crate) static BLENDER_UNREACHABLE_REGISTRATION = {
        summary: "detects a function that registers Blender classes but that `register()` never calls",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-unregister-mismatch.md")]
    pub(crate) static BLENDER_UNREGISTER_MISMATCH = {
        summary: "detects what `register()` adds and `unregister()` leaves behind, and what `unregister()` removes that was never added",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-unknown-operator.md")]
    pub(crate) static BLENDER_UNKNOWN_OPERATOR = {
        summary: "detects an operator idname that names neither an operator of the add-on nor one of Blender's",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-preferences-idname.md")]
    pub(crate) static BLENDER_PREFERENCES_IDNAME = {
        summary: "detects an `AddonPreferences` class whose `bl_idname` never makes it the add-on's preferences",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-invalid-override.md")]
    pub(crate) static BLENDER_INVALID_OVERRIDE = {
        summary: "detects a Blender callback with the wrong number of parameters, or the wrong kind of method",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-mixin-order.md")]
    pub(crate) static BLENDER_MIXIN_ORDER = {
        summary: "detects a mixin listed after a Blender base that hides the mixin's method",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-invalid-base.md")]
    pub(crate) static BLENDER_INVALID_BASE = {
        summary: "detects a class with two unrelated Blender bases",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-registered-base.md")]
    pub(crate) static BLENDER_REGISTERED_BASE = {
        summary: "detects a registered Blender class that derives from another registered class",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-missing-callback.md")]
    pub(crate) static BLENDER_MISSING_CALLBACK = {
        summary: "detects a registered Blender class without a callback Blender requires, such as a panel's `draw`",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Error,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-imprecise-operator-return.md")]
    pub(crate) static BLENDER_IMPRECISE_OPERATOR_RETURN = {
        summary: "detects an operator result typed `set[str]` where Blender takes a set of result names",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-imprecise-enum-value.md")]
    pub(crate) static BLENDER_IMPRECISE_ENUM_VALUE = {
        summary: "detects a `str` value where a Blender enum takes one of its item names",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-imprecise-callback-parameter.md")]
    pub(crate) static BLENDER_IMPRECISE_CALLBACK_PARAMETER = {
        summary: "detects a Blender callback parameter annotated narrower than the parent's, with the subclass Blender passes",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-narrowed-by-poll.md")]
    pub(crate) static BLENDER_NARROWED_BY_POLL = {
        summary: "detects a read of a pointer the class's `poll()` checked, in a callback the poll gates",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-narrowed-by-menu-prefix.md")]
    pub(crate) static BLENDER_NARROWED_BY_MENU_PREFIX = {
        summary: "detects `context.space_data` or `context.area` in a menu, typed by the editor its idname prefix names",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-narrowed-by-popover.md")]
    pub(crate) static BLENDER_NARROWED_BY_POPOVER = {
        summary: "detects `context.space_data` in a panel the add-on opens as a popover, typed by its `bl_space_type`",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-assumed-context-member.md")]
    pub(crate) static BLENDER_ASSUMED_CONTEXT_MEMBER = {
        summary: "detects `context.scene`, `view_layer` or `tool_settings` read where Blender does not guarantee them",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-assumed-list-item.md")]
    pub(crate) static BLENDER_ASSUMED_LIST_ITEM = {
        summary: "detects a read of `UIList.draw_item`'s `item`, taken as not `None`",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-assumed-registration.md")]
    pub(crate) static BLENDER_ASSUMED_REGISTRATION = {
        summary: "detects a use of a Blender class or property that `register()` registers only on a branch",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-assumed-complete-registration.md")]
    pub(crate) static BLENDER_ASSUMED_COMPLETE_REGISTRATION = {
        summary: "detects where the checker gave up following `register()` and counts every class of the add-on as registered",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-assumed-preferences.md")]
    pub(crate) static BLENDER_ASSUMED_PREFERENCES = {
        summary: "detects `addons[...].preferences` read while the add-on's preferences class is not registered",
        status: LintStatus::stable("0.0.97"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-assumed-internal-operator.md")]
    pub(crate) static BLENDER_ASSUMED_INTERNAL_OPERATOR = {
        summary: "detects a context member read in a callback of an operator whose `bl_options` has `'INTERNAL'`, taken as set",
        status: LintStatus::stable("0.0.103"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-assumed-editor-member.md")]
    pub(crate) static BLENDER_ASSUMED_EDITOR_MEMBER = {
        summary: "detects `context.area`, `region` or `space_data` read in a panel's or menu's `poll`, a tool's `draw_settings` or a viewport render callback, taken as set",
        status: LintStatus::stable("0.0.103"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-assumed-preset-context.md")]
    pub(crate) static BLENDER_ASSUMED_PRESET_CONTEXT = {
        summary: "detects a module-level read of a `bpy.context` member in a preset, taken as set by the panel that runs it",
        status: LintStatus::stable("0.0.103"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    #[doc = include_str!("../../../../resources/lint_docs/blender-assumed-added-object.md")]
    pub(crate) static BLENDER_ASSUMED_ADDED_OBJECT = {
        summary: "detects `context.object` or `active_object` read after a `bpy.ops.*_add()` call, taken as the added object",
        status: LintStatus::stable("0.0.103"),
        default_level: Level::Warn,
    }
}

pub(crate) fn register_lints(registry: &mut LintRegistryBuilder) {
    registry.register_lint(&BLENDER_INVALID_REGISTER);
    registry.register_lint(&BLENDER_INVALID_ADDON_METADATA);
    registry.register_lint(&BLENDER_INVALID_PROPERTY);
    registry.register_lint(&BLENDER_INVALID_OPERATOR_PROPERTY);
    registry.register_lint(&BLENDER_UNSUPPORTED_PROPERTY_HOST);
    registry.register_lint(&BLENDER_LEGACY_PROPERTY);
    registry.register_lint(&BLENDER_UNSUPPORTED_PROPERTY_TARGET);
    registry.register_lint(&BLENDER_REGISTER_ORDER);
    registry.register_lint(&BLENDER_DUPLICATE_REGISTRATION);
    registry.register_lint(&BLENDER_INVALID_IDNAME);
    registry.register_lint(&BLENDER_UNREGISTERED_CLASS);
    registry.register_lint(&BLENDER_UNREACHABLE_REGISTRATION);
    registry.register_lint(&BLENDER_UNREGISTER_MISMATCH);
    registry.register_lint(&BLENDER_UNKNOWN_OPERATOR);
    registry.register_lint(&BLENDER_PREFERENCES_IDNAME);
    registry.register_lint(&BLENDER_INVALID_OVERRIDE);
    registry.register_lint(&BLENDER_MIXIN_ORDER);
    registry.register_lint(&BLENDER_INVALID_BASE);
    registry.register_lint(&BLENDER_REGISTERED_BASE);
    registry.register_lint(&BLENDER_MISSING_CALLBACK);
    registry.register_lint(&BLENDER_IMPRECISE_OPERATOR_RETURN);
    registry.register_lint(&BLENDER_IMPRECISE_ENUM_VALUE);
    registry.register_lint(&BLENDER_IMPRECISE_CALLBACK_PARAMETER);
    registry.register_lint(&BLENDER_NARROWED_BY_POLL);
    registry.register_lint(&BLENDER_NARROWED_BY_MENU_PREFIX);
    registry.register_lint(&BLENDER_NARROWED_BY_POPOVER);
    registry.register_lint(&BLENDER_ASSUMED_CONTEXT_MEMBER);
    registry.register_lint(&BLENDER_ASSUMED_LIST_ITEM);
    registry.register_lint(&BLENDER_ASSUMED_REGISTRATION);
    registry.register_lint(&BLENDER_ASSUMED_COMPLETE_REGISTRATION);
    registry.register_lint(&BLENDER_ASSUMED_PREFERENCES);
    registry.register_lint(&BLENDER_ASSUMED_INTERNAL_OPERATOR);
    registry.register_lint(&BLENDER_ASSUMED_EDITOR_MEMBER);
    registry.register_lint(&BLENDER_ASSUMED_PRESET_CONTEXT);
    registry.register_lint(&BLENDER_ASSUMED_ADDED_OBJECT);
}

/// The Blender lints of `file`: the add-on lints when `file` is an add-on's root module (the
/// package's `__init__.py`, or a single-file add-on), and the property rules when it is a
/// first-party file, in an add-on or not.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn file_diagnostics<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> TypeCheckDiagnostics {
    let addon = addon_of_file(db, file).as_ref().filter(|addon| {
        file.file(db).path(db).as_system_path() == Some(addon.root_module().as_path())
    });
    let first_party = is_first_party(db, file);
    if addon.is_none() && !first_party {
        return TypeCheckDiagnostics::default();
    }
    let module = parsed_module(db, file.python_file(db)).load(db);
    let env = ProgramEnvironment::from_file(file);
    let context = InferContext::new(
        db,
        &env,
        global_scope(db, file),
        file.file(db),
        file,
        &module,
    );
    if let Some(addon) = addon {
        check_metadata(&context, addon);
        check_entry_points(&context, &env, file, &module);
    }
    if first_party {
        check_properties(&context, &module);
        check_keymaps(&context, &module);
        check_overrides(&context, file);
    }
    if first_party {
        check_assumptions(&context, file, &module);
    }
    if let Some(key) = addon_in_program(db, file) {
        report_findings(&context, registration_findings(db, key));
        report_findings(&context, &addon_preferences(db, key).problems);
    }
    context.finish().diagnostics
}

/// The registration and preferences mistakes of the add-on that are in this file.
fn report_findings(context: &InferContext, findings: &[Finding]) {
    let file = context.file();
    for finding in findings {
        if finding.file != file {
            continue;
        }
        let lint = match finding.rule {
            Rule::RegisterOrder => &BLENDER_REGISTER_ORDER,
            Rule::DuplicateRegistration => &BLENDER_DUPLICATE_REGISTRATION,
            Rule::InvalidIdname => &BLENDER_INVALID_IDNAME,
            Rule::UnregisteredClass => &BLENDER_UNREGISTERED_CLASS,
            Rule::UnreachableRegistration => &BLENDER_UNREACHABLE_REGISTRATION,
            Rule::UnregisterMismatch => &BLENDER_UNREGISTER_MISMATCH,
            Rule::PreferencesIdname => &BLENDER_PREFERENCES_IDNAME,
            Rule::RegisteredBase => &BLENDER_REGISTERED_BASE,
            Rule::MissingCallback => &BLENDER_MISSING_CALLBACK,
        };
        if let Some(builder) = context.report_lint(lint, finding.range) {
            builder.into_diagnostic(finding.message.clone());
        }
    }
}

/// The start of the root module: where problems that have no place in Python code are reported.
/// A trailing `# ty: ignore[...]` on the first logical line covers it, even after a docstring that spans
/// several lines.
fn module_start() -> TextRange {
    TextRange::empty(TextSize::default())
}

fn check_metadata(context: &InferContext, addon: &Addon) {
    let db = context.db();
    match addon.kind {
        AddonKind::LegacyPackage | AddonKind::SingleFile => {
            let Some(bl_info) = bl_info(db, context.file()) else {
                return;
            };
            let Some(dict) = &bl_info.dict else {
                if let Some(builder) =
                    context.report_lint(&BLENDER_INVALID_ADDON_METADATA, bl_info.target)
                {
                    builder.into_diagnostic("`bl_info` must be a dict display");
                }
                return;
            };
            // Reported on the name `bl_info` only, not on the dict: a range over the whole dict would also
            // match `# ty: ignore` comments on its entries.
            if !dict
                .entries
                .iter()
                .any(|entry| entry.key.as_deref() == Some("name"))
                && let Some(builder) =
                    context.report_lint(&BLENDER_INVALID_ADDON_METADATA, bl_info.target)
            {
                builder.into_diagnostic("`bl_info` has no `\"name\"` entry");
            }
            for entry in &dict.entries {
                if entry.key.as_deref() == Some("blender")
                    && !entry.is_int_triple
                    && let Some(builder) =
                        context.report_lint(&BLENDER_INVALID_ADDON_METADATA, entry.range)
                {
                    builder.into_diagnostic(
                        "`bl_info[\"blender\"]` must be a tuple of three ints, such as `(4, 2, 0)`",
                    );
                }
            }
        }
        AddonKind::Extension => {
            let Some(manifest_file) = addon.manifest_file(db) else {
                return;
            };
            let manifest = manifest(db, manifest_file);
            let report = |message: String| {
                if let Some(builder) =
                    context.report_lint(&BLENDER_INVALID_ADDON_METADATA, module_start())
                {
                    builder.into_diagnostic(message);
                }
            };
            if let Some(error) = &manifest.parse_error {
                report(format!(
                    "`blender_manifest.toml` is not valid TOML: {error}"
                ));
                return;
            }
            for key in &manifest.missing_keys {
                report(format!("`blender_manifest.toml` has no `{key}`"));
            }
            if let Some(addon_type) = &manifest.addon_type
                && addon_type != "add-on"
                && addon_type != "theme"
            {
                report(format!(
                    "`blender_manifest.toml` has `type = \"{addon_type}\"`; Blender expects `\"add-on\"` or `\"theme\"`"
                ));
            }
        }
    }
}

/// `register` and `unregister` must exist in the root module and be callable with no arguments.
fn check_entry_points(
    context: &InferContext,
    env: &ProgramEnvironment,
    file: ProgramFile,
    module: &ParsedModuleRef,
) {
    let db = context.db();
    for name in ["register", "unregister"] {
        let place = global_symbol(db, file, name).place;
        let Some(ty) = place.ignore_possibly_undefined() else {
            if let Some(builder) = context.report_lint(&BLENDER_INVALID_REGISTER, module_start()) {
                builder.into_diagnostic(format_args!(
                    "The add-on's root module has no `{name}`; Blender calls `{name}()` to enable or disable it"
                ));
            }
            continue;
        };
        if ty.is_dynamic() || ty.try_call(db, env, &CallArguments::none()).is_ok() {
            continue;
        }
        let range = binding_range(db, file, module, name).unwrap_or_else(module_start);
        if let Some(builder) = context.report_lint(&BLENDER_INVALID_REGISTER, range) {
            builder.into_diagnostic(format_args!(
                "`{name}` must be callable with no arguments; Blender calls `{name}()`"
            ));
        }
    }
}

/// The range of the last module-level binding of `name`: the `def` name, the assignment target or the
/// imported name.
fn binding_range(
    db: &dyn Db,
    file: ProgramFile,
    module: &ParsedModuleRef,
    name: &str,
) -> Option<TextRange> {
    let scope = global_scope(db, file);
    let symbol = place_table(db, scope).symbol_id(name)?;
    use_def_map(db, scope)
        .end_of_scope_symbol_bindings(symbol)
        .filter_map(|binding| binding.binding.definition())
        .last()
        .map(|definition| definition.focus_range(db, module).range())
}
