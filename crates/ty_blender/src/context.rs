//! Guaranteed context members: the members of `bpy.types.Context` that Blender always fills when
//! it calls a Blender callback, so they are never `None` there although the stubs say `X | None`.
//!
//! - `preferences`, `window_manager`, `blend_data`, `scene`, `view_layer` and `tool_settings` are
//!   filled in every callback, `Operator.execute` in background mode included: `preferences` is a
//!   static global (`rna_Context_preferences_get`), the window manager and `Main` live for the whole
//!   session, `scene` is the window's or the loaded file's scene, and the view layer and tool
//!   settings belong to the scene (`CTX_data_view_layer`, `scene_init_data`).
//! - `area`, `space_data` and `region` are filled when Blender calls the callback in an editor of the
//!   class's `bl_space_type`: a Panel or a Header is registered into that editor's region type and
//!   drawn only there, and a gizmo group is kept in that editor's gizmo map. A Menu, a UIList, an
//!   Operator or a RenderEngine has no `bl_space_type` and runs from any editor, or none.
//!
//! Only Blender calling the callback guarantees this: a helper function or `bpy.context` while an
//! add-on is imported gets no such promise, and `context.temp_override(scene=None)` breaks it.

/// When Blender fills a member of `bpy.types.Context`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ContextGuarantee {
    /// In every Blender callback.
    Always,
    /// In a callback that Blender calls in an editor of the class's `bl_space_type`
    /// ([`runs_in_editor`]).
    InEditor,
}

/// When Blender fills the member `name` of `bpy.types.Context`, or `None` when it may be `None` in
/// any callback.
pub fn context_guarantee(name: &str) -> Option<ContextGuarantee> {
    match name {
        "preferences" | "window_manager" | "blend_data" | "scene" | "view_layer"
        | "tool_settings" => Some(ContextGuarantee::Always),
        "area" | "space_data" | "region" => Some(ContextGuarantee::InEditor),
        _ => None,
    }
}

/// Whether Blender calls the callback `name` of the root `root` in an editor of the class's
/// `bl_space_type`: every callback of a Panel and a Header, and of a GizmoGroup but
/// `setup_keymap`, which runs when the key map is set up, outside any editor.
///
/// A `Gizmo` runs in its group's editor too, but that is not confirmed callback by callback, so it
/// is left out.
pub fn runs_in_editor(root: &str, name: &str) -> bool {
    match root {
        "Panel" | "Header" => true,
        "GizmoGroup" => name != "setup_keymap",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{ContextGuarantee, context_guarantee, runs_in_editor};

    #[test]
    fn guarantees() {
        assert_eq!(context_guarantee("scene"), Some(ContextGuarantee::Always));
        assert_eq!(
            context_guarantee("tool_settings"),
            Some(ContextGuarantee::Always)
        );
        assert_eq!(
            context_guarantee("space_data"),
            Some(ContextGuarantee::InEditor)
        );
        assert_eq!(context_guarantee("window"), None);
        assert_eq!(context_guarantee("object"), None);
    }

    #[test]
    fn editors() {
        assert!(runs_in_editor("Panel", "draw"));
        assert!(runs_in_editor("Header", "draw"));
        assert!(runs_in_editor("GizmoGroup", "setup"));
        assert!(!runs_in_editor("GizmoGroup", "setup_keymap"));
        assert!(!runs_in_editor("Menu", "draw"));
        assert!(!runs_in_editor("Operator", "execute"));
        assert!(!runs_in_editor("Gizmo", "draw"));
    }
}
