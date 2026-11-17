//! What a Panel's `bl_context` guarantees where Blender enforces it.
//!
//! Blender adds a top-level panel to a region only when the region's contexts name the panel's
//! `bl_context`, before it polls the panel (`panel_add_check` in `area.cc`). Only some regions pass
//! contexts, and only some of their contexts say something about the context members:
//!
//! - The Properties editor's main region passes the active tab (`object`, `physics`, ...;
//!   `buttons_main_region_context_string` in `space_buttons.cc`), and a tab is active only while
//!   its context path exists (`buttons_context_compute` in `buttons_context.cc`). The paths of the
//!   object, physics, constraint, modifier, shaderfx and bone-constraint tabs need an object, the
//!   pinned one or the active one, which is then `context.object`. The data, material and particle
//!   tabs also take pinned data, a material or particle settings without an object, so they give
//!   nothing.
//! - The 3D viewport's sidebar and toolbar pass the mode's string (`CTX_data_mode_string` in
//!   `context.cc`), so a panel there is drawn only in that mode, which gives what
//!   `context.mode == '<MODE>'` does ([`mode_facts`]). Their other contexts (`.mesh_edit`,
//!   `.paint_common`, `.objectmode`, ...) name groups of tool settings that the tool header also
//!   draws as popovers, where no context is checked: they are display-only and give nothing.

use crate::pointer_aliases::{ModeFacts, mode_facts};

/// What Blender guarantees in a panel whose (top-level) `bl_context` is `context`, with that
/// `bl_space_type` and `bl_region_type`: what is set when Blender enforces it. `None` where it
/// does not, or where the context needs nothing.
pub fn bl_context_facts(space_type: &str, region_type: &str, context: &str) -> Option<ModeFacts> {
    match (space_type, region_type) {
        ("PROPERTIES", "WINDOW") => PROPERTIES_TABS
            .iter()
            .find(|(tab, _)| *tab == context)
            .map(|(_, facts)| *facts),
        ("VIEW_3D", "UI" | "TOOLS") => mode_facts(
            MODE_STRINGS
                .iter()
                .find(|(string, _)| *string == context)?
                .1,
        ),
        _ => None,
    }
}

const fn object_tab(object_type: Option<&'static str>) -> ModeFacts {
    ModeFacts {
        set: &[&["object"]],
        object_type,
    }
}

/// The Properties tabs whose context path needs an object (`buttons_context_path` in
/// `buttons_context.cc`), with the object type the path requires.
const PROPERTIES_TABS: [(&str, ModeFacts); 6] = [
    ("object", object_tab(None)),
    ("physics", object_tab(None)),
    ("constraint", object_tab(None)),
    ("modifier", object_tab(None)),
    ("shaderfx", object_tab(Some("GREASEPENCIL"))),
    ("bone_constraint", object_tab(Some("ARMATURE"))),
];

/// The 3D viewport's mode strings (`data_mode_strings` in `context.cc`) with the `context.mode`
/// value of the same mode (`rna_enum_context_mode_items`). The legacy Grease Pencil modes and
/// `objectmode` are left out: they give nothing.
const MODE_STRINGS: [(&str, &str); 22] = [
    ("mesh_edit", "EDIT_MESH"),
    ("curve_edit", "EDIT_CURVE"),
    ("surface_edit", "EDIT_SURFACE"),
    ("text_edit", "EDIT_TEXT"),
    ("armature_edit", "EDIT_ARMATURE"),
    ("mball_edit", "EDIT_METABALL"),
    ("lattice_edit", "EDIT_LATTICE"),
    ("curves_edit", "EDIT_CURVES"),
    ("grease_pencil_edit", "EDIT_GREASE_PENCIL"),
    ("pointcloud_edit", "EDIT_POINTCLOUD"),
    ("posemode", "POSE"),
    ("sculpt_mode", "SCULPT"),
    ("weightpaint", "PAINT_WEIGHT"),
    ("vertexpaint", "PAINT_VERTEX"),
    ("imagepaint", "PAINT_TEXTURE"),
    ("particlemode", "PARTICLE"),
    ("curves_sculpt", "SCULPT_CURVES"),
    ("grease_pencil_paint", "PAINT_GREASE_PENCIL"),
    ("grease_pencil_sculpt", "SCULPT_GREASE_PENCIL"),
    ("grease_pencil_weight", "WEIGHT_GREASE_PENCIL"),
    ("grease_pencil_vertex", "VERTEX_GREASE_PENCIL"),
    ("objectmode", "OBJECT"),
];

#[cfg(test)]
mod tests {
    use super::bl_context_facts;

    #[test]
    fn properties_tabs() {
        let object = bl_context_facts("PROPERTIES", "WINDOW", "object");
        assert!(object.is_some_and(|facts| facts.set == [&["object"][..]]));
        assert!(bl_context_facts("PROPERTIES", "WINDOW", "physics").is_some());
        assert!(
            bl_context_facts("PROPERTIES", "WINDOW", "bone_constraint")
                .is_some_and(|facts| facts.object_type == Some("ARMATURE"))
        );
        // Pinned data, a material or particle settings have no object.
        assert_eq!(bl_context_facts("PROPERTIES", "WINDOW", "data"), None);
        assert_eq!(bl_context_facts("PROPERTIES", "WINDOW", "material"), None);
        assert_eq!(bl_context_facts("VIEW_3D", "UI", "object"), None);
    }

    #[test]
    fn modes() {
        let mesh_edit = bl_context_facts("VIEW_3D", "UI", "mesh_edit");
        assert!(mesh_edit.is_some_and(|facts| facts.object_type == Some("MESH")));
        assert!(
            bl_context_facts("VIEW_3D", "TOOLS", "posemode")
                .is_some_and(|facts| facts.set.contains(&&["object", "pose"][..]))
        );
        assert_eq!(bl_context_facts("VIEW_3D", "UI", "objectmode"), None);
        assert_eq!(bl_context_facts("VIEW_3D", "UI", ".mesh_edit"), None);
        assert_eq!(bl_context_facts("VIEW_3D", "UI", ".objectmode"), None);
        assert_eq!(bl_context_facts("VIEW_3D", "HEADER", "mesh_edit"), None);
        assert_eq!(bl_context_facts("IMAGE_EDITOR", "UI", "mesh_edit"), None);
    }
}
