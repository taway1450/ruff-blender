//! Facts about Blender add-ons that need no types.
//!
//! Which add-on a file belongs to and which files an add-on has, read from `blender_manifest.toml`, a
//! top-level `bl_info` and `[tool.blender]` in `pyproject.toml`; which classes Blender registers and
//! which idnames it refuses, and which callbacks it calls on them; what an operator call takes and
//! returns, and which Blender functions take an operator idname; which pointers Blender keeps in step
//! and what a mode or a panel's `bl_context` implies; which code runs only when Blender reloads an
//! add-on, and which never runs in one; which test-case assertions narrow like `assert`; which Type
//! tags a test may not narrow by, and which constructors take a `bl_idname`. The type-aware glue lives
//! in `ty_python_semantic`'s `types/dedicated/blender/`.

mod assertions;
mod bl_context;
mod callbacks;
mod context;
mod discovery;
mod external;
mod files;
mod metadata;
mod operators;
mod pointer_aliases;
mod registration;
mod reload_guard;
mod script_guard;
mod source;
mod type_tags;

pub use assertions::{UnittestAssertion, unittest_assertion};
pub use bl_context::bl_context_facts;
pub use callbacks::{
    CALLBACKS, Callback, CallbackKind, CallbackShape, MethodKind, PYTHON_BASE_NAMES, callback_of,
    callbacks_of, gated_by_poll, is_callback_name, is_callback_root, problem, python_base_names,
};
pub use context::{
    ContextGuarantee, adds_object, assumes_editor, checked_by_call, context_guarantee,
    draws_in_window, is_added_object_member, is_assumed_context_member, is_editor_member,
    is_preset_path, is_tool_draw_settings, menu_space_type, preset_member_struct, runs_in_editor,
};
pub use discovery::{Addon, AddonKind, addon_for_path};
pub use external::{DEFAULT_ADDONS, STARTUP_PACKAGES, module_addon};
pub use files::{addon_source_files, folder_source_files};
pub use metadata::{MANIFEST_FILE, Manifest, REQUIRED_MANIFEST_KEYS, manifest};
pub use operators::{
    EXECUTION_CONTEXTS, OPERATOR_RESULTS, OperatorIdname, SINKS, Sink, SinkResult,
};
pub use pointer_aliases::{ModeFacts, implied_paths, is_set_once_read, mode_facts, same_pointers};
pub use registration::{IdnameFamily, REGISTRABLE_ROOTS, registration_classmethod};
pub use reload_guard::{imports_submodule_by_name, in_reload_guard};
pub use script_guard::is_script_test;
pub use source::{BlInfo, BlInfoDict, BlInfoEntry, bl_info};
pub use type_tags::{
    ADDON_TYPE_TAG, IdnameConstructor, NO_SUBCLASS_MEMBERS, PointerTag, excludes_on_mismatch,
    idname_constructor, object_types_of_data, pointer_tag, tag_writers,
};
