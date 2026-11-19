//! Pointer aliases and modes: what one pointer of the context being set, or the context's mode,
//! says about other pointers, by Blender's own C code. Each path is a member chain from the
//! context (`["space_data", "edit_tree"]` is `context.space_data.edit_tree`).

/// A guarantee Blender keeps between two context paths.
enum PointerAlias {
    /// The first path being set means the second is set.
    Implies(&'static [&'static str], &'static [&'static str]),
    /// The two paths are the same pointer.
    Same(&'static [&'static str], &'static [&'static str]),
}

/// The Pointer aliases of the node editor (`space_node.cc`):
///
/// - `ED_node_tree_start` sets `node_tree` and `edit_tree` to the same tree, `ED_node_tree_push`
///   and `ED_node_tree_pop` keep `edit_tree` the last entry of the tree path, whose root is never
///   popped, and `node_id_remap` clears both together. The `node_tree` setter goes through
///   `ED_node_tree_start`. So a set `node_tree` means a set `edit_tree` (the tree itself or a group
///   inside it).
/// - `context.active_node` is provided by the node editor only, as
///   `node_get_active(*snode->edittree)`: the edited tree's active node.
///
/// And of the context (`buttons_context.cc`, `screen_context.cc`):
///
/// - `context.particle_system` is provided by the Properties editor only, from a context path
///   through an object (`buttons_context_path_particle`), which is then `context.object`; pinned
///   particle settings give `particle_settings` instead.
/// - `context.active_strip` is the active strip of `CTX_data_sequencer_scene`
///   (`screen_ctx_active_strip`), the scene `context.sequencer_scene` gives.
///
/// Not aliases: `active_pose_bone` and `active_object` (the 3D viewport leaves `active_object`
/// `None` for a hidden object in object mode, `view3d_context.cc`, while the pose bone may be of
/// the armature that deforms it, `BKE_object_pose_armature_get`), and `tool_settings` and
/// `scene.tool_settings` (the sequencer gives the sequencer scene's, `sequencer_context`).
const POINTER_ALIASES: [PointerAlias; 4] = [
    PointerAlias::Implies(&["space_data", "node_tree"], &["space_data", "edit_tree"]),
    PointerAlias::Same(
        &["active_node"],
        &["space_data", "edit_tree", "nodes", "active"],
    ),
    PointerAlias::Implies(&["particle_system"], &["object"]),
    PointerAlias::Implies(&["active_strip"], &["sequencer_scene"]),
];

/// The paths that are set whenever the context path `path` is: the other sides of its Pointer
/// aliases, and the members on the way to them (`space_data.edit_tree.nodes.active` being set means
/// `space_data` and `space_data.edit_tree` are).
pub fn implied_paths(path: &[impl AsRef<str>]) -> Vec<&'static [&'static str]> {
    let mut implied: Vec<&'static [&'static str]> = Vec::new();
    for alias in &POINTER_ALIASES {
        let target = match alias {
            PointerAlias::Implies(checked, target) if same_path(path, checked) => *target,
            PointerAlias::Same(one, other) if same_path(path, one) => *other,
            PointerAlias::Same(one, other) if same_path(path, other) => *one,
            _ => continue,
        };
        for end in 1..=target.len() {
            let prefix = &target[..end];
            if !implied.contains(&prefix) {
                implied.push(prefix);
            }
        }
    }
    implied
}

/// The context paths that are the same pointer as `path`, so that a Type tag test of one is a test
/// of the other (`context.active_node.type` and `space_data.edit_tree.nodes.active.type`).
pub fn same_pointers(path: &[impl AsRef<str>]) -> Vec<&'static [&'static str]> {
    POINTER_ALIASES
        .iter()
        .filter_map(|alias| match alias {
            PointerAlias::Same(one, other) if same_path(path, one) => Some(*other),
            PointerAlias::Same(one, other) if same_path(path, other) => Some(*one),
            _ => None,
        })
        .collect()
}

/// Whether a read of the context path `path` is never `None` once its receiver was read: the
/// `gpencil_settings` of the brush of a Grease Pencil paint mode
/// (`tool_settings.gpencil_paint.brush.gpencil_settings`).
///
/// `Paint.brush` is read-only, and `BKE_paint_brush_set` only takes a brush whose `ob_mode` has
/// the paint's mode (`BKE_paint_can_use_brush`). `BKE_brush_add` allocates `gpencil_settings` for
/// the four Grease Pencil modes, `bpy.data.brushes.new()` for every brush, and entering Grease
/// Pencil draw mode ensures them for its brush. `rna_brush.cc` lets a script set a Grease Pencil
/// bit on `ob_mode` (`use_paint_grease_pencil`, `use_vertex_grease_pencil`) without allocating
/// them, which only matters for a brush loaded without them that a script gives such a mode.
pub fn is_set_once_read(path: &[impl AsRef<str>]) -> bool {
    let [.., paint, brush, settings] = path else {
        return false;
    };
    settings.as_ref() == "gpencil_settings"
        && brush.as_ref() == "brush"
        && GREASE_PENCIL_PAINTS.contains(&paint.as_ref())
}

/// The members of `ToolSettings` that hold a Grease Pencil paint mode's settings.
const GREASE_PENCIL_PAINTS: [&str; 4] = [
    "gpencil_paint",
    "gpencil_sculpt_paint",
    "gpencil_weight_paint",
    "gpencil_vertex_paint",
];

/// What `context.mode` being `mode` says about the context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeFacts {
    /// The context paths that are set.
    pub set: &'static [&'static [&'static str]],
    /// The `Object.type` of the objects among them, its one-member paths (`context.object`,
    /// `active_object` and, in an edit mode, `edit_object`), when the mode names one.
    pub object_type: Option<&'static str>,
}

/// What `context.mode` being `mode` says about the context, from `CTX_data_mode_enum_ex`
/// (`context.cc`): an edit mode is the edit object's, so `object`, `active_object` and
/// `edit_object` are set with that object type; a paint, sculpt or pose mode is the active
/// object's, and entering it ensures its tool settings (`BKE_paint_ensure`); pose mode is an
/// armature's, whose `pose` is set. `None` for `OBJECT` and the legacy Grease Pencil modes.
pub fn mode_facts(mode: &str) -> Option<ModeFacts> {
    MODES
        .iter()
        .find(|(name, _)| *name == mode)
        .map(|(_, facts)| *facts)
}

const EDIT_OBJECTS: &[&[&str]] = &[&["object"], &["active_object"], &["edit_object"]];

const fn edit(object_type: &'static str) -> ModeFacts {
    ModeFacts {
        set: EDIT_OBJECTS,
        object_type: Some(object_type),
    }
}

const MODES: [(&str, ModeFacts); 21] = [
    ("EDIT_MESH", edit("MESH")),
    ("EDIT_CURVE", edit("CURVE")),
    ("EDIT_CURVES", edit("CURVES")),
    ("EDIT_SURFACE", edit("SURFACE")),
    ("EDIT_TEXT", edit("FONT")),
    ("EDIT_ARMATURE", edit("ARMATURE")),
    ("EDIT_METABALL", edit("META")),
    ("EDIT_LATTICE", edit("LATTICE")),
    ("EDIT_GREASE_PENCIL", edit("GREASEPENCIL")),
    ("EDIT_POINTCLOUD", edit("POINTCLOUD")),
    (
        "POSE",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["object", "pose"],
                &["active_object", "pose"],
            ],
            object_type: Some("ARMATURE"),
        },
    ),
    (
        "SCULPT",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "sculpt"],
            ],
            object_type: Some("MESH"),
        },
    ),
    (
        "PAINT_WEIGHT",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "weight_paint"],
            ],
            object_type: Some("MESH"),
        },
    ),
    (
        "PAINT_VERTEX",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "vertex_paint"],
            ],
            object_type: Some("MESH"),
        },
    ),
    (
        "PAINT_TEXTURE",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "image_paint"],
            ],
            object_type: Some("MESH"),
        },
    ),
    (
        "PARTICLE",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "particle_edit"],
            ],
            object_type: None,
        },
    ),
    (
        "SCULPT_CURVES",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "curves_sculpt"],
            ],
            object_type: Some("CURVES"),
        },
    ),
    (
        "PAINT_GREASE_PENCIL",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "gpencil_paint"],
            ],
            object_type: Some("GREASEPENCIL"),
        },
    ),
    (
        "SCULPT_GREASE_PENCIL",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "gpencil_sculpt_paint"],
            ],
            object_type: Some("GREASEPENCIL"),
        },
    ),
    (
        "WEIGHT_GREASE_PENCIL",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "gpencil_weight_paint"],
            ],
            object_type: Some("GREASEPENCIL"),
        },
    ),
    (
        "VERTEX_GREASE_PENCIL",
        ModeFacts {
            set: &[
                &["object"],
                &["active_object"],
                &["tool_settings", "gpencil_vertex_paint"],
            ],
            object_type: Some("GREASEPENCIL"),
        },
    ),
];

fn same_path(path: &[impl AsRef<str>], other: &[&str]) -> bool {
    path.len() == other.len()
        && path
            .iter()
            .zip(other)
            .all(|(name, other)| name.as_ref() == *other)
}

#[cfg(test)]
mod tests {
    use super::{implied_paths, is_set_once_read, mode_facts, same_pointers};

    #[test]
    fn node_editor() {
        assert_eq!(
            implied_paths(&["space_data", "node_tree"]),
            vec![&["space_data"][..], &["space_data", "edit_tree"][..]]
        );
        assert!(implied_paths(&["space_data", "edit_tree"]).is_empty());
        assert_eq!(implied_paths(&["active_node"]).len(), 4);
        assert_eq!(
            implied_paths(&["space_data", "edit_tree", "nodes", "active"]),
            vec![&["active_node"][..]]
        );
        assert_eq!(
            same_pointers(&["active_node"]),
            vec![&["space_data", "edit_tree", "nodes", "active"][..]]
        );
        assert!(same_pointers(&["space_data", "node_tree"]).is_empty());
    }

    #[test]
    fn context_pointers() {
        assert_eq!(implied_paths(&["particle_system"]), vec![&["object"][..]]);
        assert_eq!(
            implied_paths(&["active_strip"]),
            vec![&["sequencer_scene"][..]]
        );
        assert!(implied_paths(&["object"]).is_empty());
        assert!(implied_paths(&["active_pose_bone"]).is_empty());
    }

    #[test]
    fn brush_settings() {
        assert!(is_set_once_read(&[
            "tool_settings",
            "gpencil_paint",
            "brush",
            "gpencil_settings"
        ]));
        assert!(is_set_once_read(&[
            "scene",
            "tool_settings",
            "gpencil_vertex_paint",
            "brush",
            "gpencil_settings"
        ]));
        assert!(!is_set_once_read(&[
            "tool_settings",
            "sculpt",
            "brush",
            "gpencil_settings"
        ]));
        assert!(!is_set_once_read(&[
            "tool_settings",
            "gpencil_paint",
            "brush"
        ]));
    }

    #[test]
    fn modes() {
        let edit_mesh = mode_facts("EDIT_MESH").unwrap_or_else(|| panic!("EDIT_MESH"));
        assert_eq!(edit_mesh.object_type, Some("MESH"));
        assert!(edit_mesh.set.contains(&&["edit_object"][..]));
        let sculpt = mode_facts("SCULPT").unwrap_or_else(|| panic!("SCULPT"));
        assert!(sculpt.set.contains(&&["tool_settings", "sculpt"][..]));
        assert!(!sculpt.set.contains(&&["edit_object"][..]));
        assert!(mode_facts("POSE").is_some_and(|pose| pose.set.contains(&&["object", "pose"][..])));
        assert_eq!(mode_facts("OBJECT"), None);
    }
}
