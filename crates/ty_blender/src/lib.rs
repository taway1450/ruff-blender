//! Facts about Blender add-ons that need no types.
//!
//! Which add-on a file belongs to and which files an add-on has, read from `blender_manifest.toml`, a
//! top-level `bl_info` and `[tool.blender]` in `pyproject.toml`; which classes Blender registers and which
//! idnames it refuses; what an operator call takes and returns. The type-aware glue lives in
//! `ty_python_semantic`'s `types/dedicated/blender/`.

mod discovery;
mod files;
mod metadata;
mod operators;
mod registration;
mod source;

pub use discovery::{Addon, AddonKind, addon_for_path};
pub use files::addon_source_files;
pub use metadata::{MANIFEST_FILE, Manifest, REQUIRED_MANIFEST_KEYS, manifest};
pub use operators::{EXECUTION_CONTEXTS, OPERATOR_RESULTS, OperatorIdname};
pub use registration::{IdnameFamily, REGISTRABLE_ROOTS};
pub use source::{BlInfo, BlInfoDict, BlInfoEntry, bl_info};
