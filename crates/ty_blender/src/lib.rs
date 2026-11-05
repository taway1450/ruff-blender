//! Facts about Blender add-ons that need no types.
//!
//! Which add-on a file belongs to, read from `blender_manifest.toml`, a top-level `bl_info` and
//! `[tool.blender]` in `pyproject.toml`. The type-aware glue lives in `ty_python_semantic`'s
//! `types/dedicated/blender/`.

mod discovery;
mod metadata;
mod source;

pub use discovery::{Addon, AddonKind, addon_for_path};
pub use metadata::{MANIFEST_FILE, Manifest, REQUIRED_MANIFEST_KEYS, manifest};
pub use source::{BlInfo, BlInfoDict, BlInfoEntry, bl_info};
