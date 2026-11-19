//! The add-ons and packages outside an add-on whose registrations it may read.
//!
//! Blender shares one `bpy.types` between everything it loads, so `bpy.types.Scene.cycles`, which
//! Cycles registers, is an attribute of every scene while Cycles is enabled. Three kinds of module
//! register properties another add-on may read:
//!
//! - the add-ons an add-on declares in its manifest (`[tool.blender] uses = ["cycles"]`);
//! - the startup packages Blender always loads ([`STARTUP_PACKAGES`]);
//! - the add-ons Blender enables by default ([`DEFAULT_ADDONS`]), which a user can disable.

use ruff_db::Db;
use ruff_db::system::SystemPath;

use crate::discovery::{Addon, AddonKind, existing_file};
use crate::metadata::MANIFEST_FILE;

/// The packages under `scripts/startup` that register properties: Blender imports and registers
/// them at every start (`bl_operators.bone_selection_sets` registers `Object.selection_sets`).
pub const STARTUP_PACKAGES: [&str; 2] = ["bl_operators", "bl_ui"];

/// The add-ons Blender enables in its factory settings (`BKE_blendfile_userdef_from_defaults` in
/// `blendfile.cc`).
pub const DEFAULT_ADDONS: [&str; 8] = [
    "cycles",
    "io_anim_bvh",
    "io_curve_svg",
    "io_mesh_uv_layout",
    "io_scene_fbx",
    "io_scene_gltf2",
    "pose_library",
    "bl_pkg",
];

/// The add-on Blender imports as the module `name` from the file `module`: the package around an
/// `__init__.py` (an extension when it has a manifest), or the single file.
pub fn module_addon(db: &dyn Db, name: &str, module: &SystemPath) -> Option<Addon> {
    if module.file_name() == Some("__init__.py") {
        let root = module.parent()?;
        let kind = if existing_file(db, &root.join(MANIFEST_FILE)).is_some() {
            AddonKind::Extension
        } else {
            AddonKind::LegacyPackage
        };
        return Some(Addon {
            kind,
            root: root.to_path_buf(),
            name: name.to_string(),
        });
    }
    Some(Addon {
        kind: AddonKind::SingleFile,
        root: module.to_path_buf(),
        name: name.to_string(),
    })
}
