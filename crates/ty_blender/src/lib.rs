//! Facts about Blender add-ons that need no types.
//!
//! Which add-on a file belongs to and which files an add-on has, read from `blender_manifest.toml`, a
//! top-level `bl_info` and `[tool.blender]` in `pyproject.toml`; which classes Blender registers and which
//! idnames it refuses, and which callbacks it calls on them; what an operator call takes and returns, and which Blender
//! functions take an operator idname; which code runs only when Blender reloads an add-on, and which
//! never runs in one; which Type tags a test may not narrow by. The
//! type-aware glue lives in `ty_python_semantic`'s `types/dedicated/blender/`.

mod callbacks;
mod context;
mod discovery;
mod files;
mod metadata;
mod operators;
mod registration;
mod reload_guard;
mod script_guard;
mod source;
mod type_tags;

pub use callbacks::{
    CALLBACKS, Callback, CallbackKind, CallbackShape, MethodKind, PYTHON_BASE_NAMES, callback_of,
    callbacks_of, is_callback_name, is_callback_root, problem, python_base_names,
};
pub use context::{ContextGuarantee, context_guarantee, runs_in_editor};
pub use discovery::{Addon, AddonKind, addon_for_path};
pub use files::addon_source_files;
pub use metadata::{MANIFEST_FILE, Manifest, REQUIRED_MANIFEST_KEYS, manifest};
pub use operators::{
    EXECUTION_CONTEXTS, OPERATOR_RESULTS, OperatorIdname, SINKS, Sink, SinkResult,
};
pub use registration::{IdnameFamily, REGISTRABLE_ROOTS};
pub use reload_guard::in_reload_guard;
pub use script_guard::is_script_test;
pub use source::{BlInfo, BlInfoDict, BlInfoEntry, bl_info};
pub use type_tags::{ADDON_TYPE_TAG, excludes_on_mismatch};
