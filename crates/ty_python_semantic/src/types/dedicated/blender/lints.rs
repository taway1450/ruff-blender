//! The add-on lints: `blender-invalid-register` and `blender-invalid-addon-metadata`.
//!
//! They describe the add-on as a whole, not one expression, so they are reported by a separate query on
//! the add-on's root module, which `check_types` merges before it checks suppressions. That keeps
//! `# ty: ignore[...]` working for them, and keeps a manifest edit from re-inferring the module.

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

use super::addon_of_file;

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

pub(crate) fn register_lints(registry: &mut LintRegistryBuilder) {
    registry.register_lint(&BLENDER_INVALID_REGISTER);
    registry.register_lint(&BLENDER_INVALID_ADDON_METADATA);
}

/// The add-on lints of `file`. Empty unless `file` is an add-on's root module: the package's
/// `__init__.py`, or a single-file add-on.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn addon_diagnostics<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> TypeCheckDiagnostics {
    let Some(addon) = addon_of_file(db, file) else {
        return TypeCheckDiagnostics::default();
    };
    if file.file(db).path(db).as_system_path() != Some(addon.root_module().as_path()) {
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
    check_metadata(&context, addon);
    check_entry_points(&context, &env, file, &module);
    context.finish().diagnostics
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
