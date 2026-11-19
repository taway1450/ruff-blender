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
//!   `context.mode == '<MODE>'` does ([`mode_facts`]). The sidebar also passes the dotted
//!   tool-settings group of the mode (`.mesh_edit`, `.paint_common`, ...;
//!   `ED_view3d_buttons_contexts` in `space_view3d.cc`), and the tool header draws a group as
//!   popovers only under a test of the same mode (`VIEW3D_HT_tool_header` in `space_view3d.py`),
//!   so a group gives what its modes share ([`DOTTED_GROUPS`]); `.objectmode` gives nothing.

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
        ("VIEW_3D", "UI") if context.starts_with('.') => {
            match DOTTED_GROUPS.iter().find(|(group, _)| *group == context)?.1 {
                Group::Mode(mode) => mode_facts(mode),
                Group::Shared(facts) => Some(facts),
            }
        }
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

/// What a dotted tool-settings group gives: the facts of its one mode, or what its modes share.
#[derive(Clone, Copy)]
enum Group {
    Mode(&'static str),
    Shared(ModeFacts),
}

/// The dotted groups of the 3D viewport's sidebar, by the modes `ED_view3d_buttons_contexts`
/// (`space_view3d.cc`) passes them in. `.curve_edit` is passed in curve and surface edit mode,
/// which share an edit object of no one type; `.paint_common` in sculpt, weight, vertex and
/// texture paint, particle edit and curves sculpt mode, which share an active object of no one
/// type. `.greasepencil_weight` and `.greasepencil_vertex` are also passed in the legacy Grease
/// Pencil modes, which `CTX_data_mode_enum_ex` no longer returns. `.objectmode` and the legacy
/// `.greasepencil_edit` are left out: they give nothing.
const DOTTED_GROUPS: [(&str, Group); 20] = [
    (".mesh_edit", Group::Mode("EDIT_MESH")),
    (
        ".curve_edit",
        Group::Shared(ModeFacts {
            set: &[&["object"], &["active_object"], &["edit_object"]],
            object_type: None,
        }),
    ),
    (".curves_edit", Group::Mode("EDIT_CURVES")),
    (".text_edit", Group::Mode("EDIT_TEXT")),
    (".armature_edit", Group::Mode("EDIT_ARMATURE")),
    (".mball_edit", Group::Mode("EDIT_METABALL")),
    (".lattice_edit", Group::Mode("EDIT_LATTICE")),
    (".grease_pencil_edit", Group::Mode("EDIT_GREASE_PENCIL")),
    (".grease_pencil_paint", Group::Mode("PAINT_GREASE_PENCIL")),
    (".grease_pencil_sculpt", Group::Mode("SCULPT_GREASE_PENCIL")),
    (".greasepencil_weight", Group::Mode("WEIGHT_GREASE_PENCIL")),
    (".greasepencil_vertex", Group::Mode("VERTEX_GREASE_PENCIL")),
    (".pointcloud_edit", Group::Mode("EDIT_POINTCLOUD")),
    (".posemode", Group::Mode("POSE")),
    (
        ".paint_common",
        Group::Shared(ModeFacts {
            set: &[&["object"], &["active_object"]],
            object_type: None,
        }),
    ),
    (".sculpt_mode", Group::Mode("SCULPT")),
    (".weightpaint", Group::Mode("PAINT_WEIGHT")),
    (".vertexpaint", Group::Mode("PAINT_VERTEX")),
    (".imagepaint", Group::Mode("PAINT_TEXTURE")),
    (".particlemode", Group::Mode("PARTICLE")),
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
        assert!(
            bl_context_facts("VIEW_3D", "UI", ".mesh_edit")
                .is_some_and(|facts| facts.object_type == Some("MESH"))
        );
        assert!(
            bl_context_facts("VIEW_3D", "UI", ".sculpt_mode")
                .is_some_and(|facts| facts.set.contains(&&["tool_settings", "sculpt"][..]))
        );
        assert!(
            bl_context_facts("VIEW_3D", "UI", ".paint_common").is_some_and(|facts| {
                facts.object_type.is_none() && !facts.set.contains(&&["edit_object"][..])
            })
        );
        assert!(
            bl_context_facts("VIEW_3D", "UI", ".curve_edit")
                .is_some_and(|facts| facts.set.contains(&&["edit_object"][..]))
        );
        // The toolbar passes the mode's string only.
        assert_eq!(bl_context_facts("VIEW_3D", "TOOLS", ".mesh_edit"), None);
        assert_eq!(bl_context_facts("VIEW_3D", "UI", ".objectmode"), None);
        assert_eq!(bl_context_facts("VIEW_3D", "HEADER", "mesh_edit"), None);
        assert_eq!(bl_context_facts("IMAGE_EDITOR", "UI", "mesh_edit"), None);
    }
}
