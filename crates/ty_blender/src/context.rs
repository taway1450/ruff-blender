//! Guaranteed context members: the members of `bpy.types.Context` that Blender always fills when
//! it calls a Blender callback, so they are never `None` there although the stubs say `X | None`.
//!
//! - `preferences`, `window_manager`, `blend_data`, `scene`, `view_layer`, `tool_settings` and
//!   `collection` are filled in every callback, `Operator.execute` in background mode included:
//!   `preferences` is a static global (`rna_Context_preferences_get`), the window manager and `Main`
//!   live for the whole session, `scene` is the window's or the loaded file's scene, and the view
//!   layer and tool settings belong to the scene (`CTX_data_view_layer`, `scene_init_data`). The
//!   collection is the active layer collection's, else the scene's master collection
//!   (`CTX_data_collection`), so it is `None` exactly when `scene` is.
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
        | "tool_settings" | "collection" => Some(ContextGuarantee::Always),
        "area" | "space_data" | "region" => Some(ContextGuarantee::InEditor),
        _ => None,
    }
}

/// Whether the member `name` of `bpy.types.Context` is an assumed one outside Blender callbacks:
/// `scene`, `view_layer`, `tool_settings` and `collection` hold the window's or the loaded file's
/// scene (or its master collection) wherever
/// Python code runs in ordinary use (a timer runs with the first window set, an app handler, a
/// `--python` script, a test or a preset with the loaded file's scene), and are `None` only under
/// `context.temp_override(scene=None)` or in a FileHandler's import operator run by the collection
/// importer. While an add-on is imported or registered, `bpy.context` has no `scene` at all: it
/// raises, which no type expresses.
///
/// The other Guaranteed members are left out: `preferences` and `window_manager` are never `None`
/// in the stubs, and `blend_data` is not read outside callbacks in practice.
pub fn is_assumed_context_member(name: &str) -> bool {
    matches!(name, "scene" | "view_layer" | "tool_settings" | "collection")
}

/// Whether the member `name` of `bpy.types.Context` is one of the editor's: `area`, `region` and
/// `space_data`, set when Blender calls the code while it draws an editor.
pub fn is_editor_member(name: &str) -> bool {
    matches!(name, "area" | "region" | "space_data")
}

/// Whether Blender calls the callback `name` of the root `root` while it draws an editor, though the
/// class names no editor of its own: a Panel's, Menu's or asset shelf's `poll`, which Blender runs
/// while drawing a region (`ED_region_panels`, `UI_menutype_draw`), and `RenderEngine.view_update`
/// and `view_draw`, called for the 3D viewport that shows the render. A script, `wm.call_menu` or
/// `wm.call_panel` can poll a menu or panel with no editor, so the editor members there are an
/// Assumption.
pub fn assumes_editor(root: &str, name: &str) -> bool {
    match root {
        "Panel" | "Menu" | "AssetShelf" => name == "poll",
        "RenderEngine" => matches!(name, "view_update" | "view_draw"),
        _ => false,
    }
}

/// Whether a function named `name` draws a tool's settings: `WorkSpaceTool.draw_settings`, or a
/// function the tool system takes as one (`draw_settings`, `draw_settings_interactive_add` in
/// `bl_ui`'s tool definitions). Blender calls it while drawing the tool header or the tool settings
/// of an editor.
pub fn is_tool_draw_settings(name: &str) -> bool {
    name == "draw_settings" || name.starts_with("draw_settings_")
}

/// Whether the member `name` of `bpy.types.Context` is the active object an operator that adds an
/// object makes the new one ([`adds_object`]).
pub fn is_added_object_member(name: &str) -> bool {
    matches!(name, "object" | "active_object")
}

/// Whether the operator `bpy.ops.<category>.<name>` is taken to add an object and make it the
/// active one: `object.add` and every `*_add` operator (`mesh.primitive_cube_add`,
/// `object.light_add`). `ED_object_add_type` selects the new object and makes it active; whether the
/// call succeeded is not visible in the code, so this is an Assumption.
pub fn adds_object(name: &str) -> bool {
    name == "add" || name.ends_with("_add")
}

/// Whether the file at `path` is a preset: a Python file in a folder named `presets`
/// (`scripts/presets/camera/1_inch.py`, an add-on's `presets/<subdir>/`). Blender runs a preset with
/// the context of the panel or menu that applies it (`script.execute_preset`).
pub fn is_preset_path(path: &str) -> bool {
    path.split(['/', '\\'])
        .rev()
        .skip(1)
        .any(|folder| folder == "presets")
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

/// The `bl_space_type` of the editor a menu with the idname `idname` is drawn in, by the prefix
/// Blender's naming convention gives it: `<PREFIX>_MT_<name>`, `<PREFIX>` being one of the editors'
/// header prefixes (`CLIP_MT_view` is drawn in the clip editor, as its header is `CLIP_HT_header`).
/// Menu search relies on the same convention (each editor's root menu is
/// `<PREFIX>_MT_editor_menus`). `TIME` and `ASSETBROWSER` are modes of the dope sheet and the file
/// browser. `None` for any other idname: a data prefix (`OBJECT_MT_`, `MASK_MT_`) or an add-on's own.
///
/// A menu has no `bl_space_type` and can be drawn from any editor (`wm.call_menu` in a user key
/// map), so this is a convention, not a guarantee.
pub fn menu_space_type(idname: &str) -> Option<&'static str> {
    let (prefix, _) = idname.split_once("_MT_")?;
    MENU_PREFIXES
        .iter()
        .find(|(menu_prefix, _)| *menu_prefix == prefix)
        .map(|(_, space_type)| *space_type)
}

/// The menu idname prefixes and the `bl_space_type` they name: the 19 header prefixes of
/// Blender's editors (`<PREFIX>_HT_header`), and `TIME` and `ASSETBROWSER`.
const MENU_PREFIXES: [(&str, &str); 21] = [
    ("VIEW3D", "VIEW_3D"),
    ("IMAGE", "IMAGE_EDITOR"),
    ("NODE", "NODE_EDITOR"),
    ("SEQUENCER", "SEQUENCE_EDITOR"),
    ("CLIP", "CLIP_EDITOR"),
    ("DOPESHEET", "DOPESHEET_EDITOR"),
    ("GRAPH", "GRAPH_EDITOR"),
    ("NLA", "NLA_EDITOR"),
    ("TEXT", "TEXT_EDITOR"),
    ("CONSOLE", "CONSOLE"),
    ("INFO", "INFO"),
    ("OUTLINER", "OUTLINER"),
    ("PROPERTIES", "PROPERTIES"),
    ("FILEBROWSER", "FILE_BROWSER"),
    ("SPREADSHEET", "SPREADSHEET"),
    ("USERPREF", "PREFERENCES"),
    ("PROJECT", "PROJECT"),
    ("TOPBAR", "TOPBAR"),
    ("STATUSBAR", "STATUSBAR"),
    ("TIME", "DOPESHEET_EDITOR"),
    ("ASSETBROWSER", "FILE_BROWSER"),
];

/// The context path that the `bpy.types` function `class.function`, called with the context,
/// checks when it returns true: the member chain from the context that is then not `None`.
///
/// `XrSessionState.is_running(context)` is true only while the window manager's XR session
/// exists (`WM_xr_session_exists`), and `window_manager.xr_session_state` is that session's state
/// (`rna_WindowManager_xr_session_state_get`), `None` only without one.
pub fn checked_by_call(class: &str, function: &str) -> Option<&'static [&'static str]> {
    CHECKED_BY_CALL
        .iter()
        .find(|(known_class, known_function, _)| {
            *known_class == class && *known_function == function
        })
        .map(|(_, _, path)| *path)
}

const CHECKED_BY_CALL: [(&str, &str, &[&str]); 1] = [(
    "XrSessionState",
    "is_running",
    &["window_manager", "xr_session_state"],
)];

#[cfg(test)]
mod tests {
    use super::{
        ContextGuarantee, adds_object, assumes_editor, context_guarantee, is_added_object_member,
        is_assumed_context_member, is_editor_member, is_preset_path, is_tool_draw_settings,
        menu_space_type, runs_in_editor,
    };

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
        assert_eq!(
            context_guarantee("collection"),
            Some(ContextGuarantee::Always)
        );
        assert_eq!(context_guarantee("window"), None);
        assert_eq!(context_guarantee("object"), None);
    }

    #[test]
    fn assumed_members() {
        assert!(is_assumed_context_member("scene"));
        assert!(is_assumed_context_member("view_layer"));
        assert!(is_assumed_context_member("tool_settings"));
        assert!(is_assumed_context_member("collection"));
        assert!(!is_assumed_context_member("blend_data"));
        assert!(!is_assumed_context_member("object"));
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

    #[test]
    fn editor_members() {
        assert!(is_editor_member("region"));
        assert!(is_editor_member("space_data"));
        assert!(!is_editor_member("window"));
        assert!(assumes_editor("Panel", "poll"));
        assert!(assumes_editor("Menu", "poll"));
        assert!(assumes_editor("RenderEngine", "view_draw"));
        assert!(!assumes_editor("Menu", "draw"));
        assert!(!assumes_editor("RenderEngine", "render"));
        assert!(!assumes_editor("Operator", "poll"));
        assert!(is_tool_draw_settings("draw_settings"));
        assert!(is_tool_draw_settings("draw_settings_interactive_add"));
        assert!(!is_tool_draw_settings("draw"));
    }

    #[test]
    fn added_objects() {
        assert!(adds_object("primitive_cube_add"));
        assert!(adds_object("add"));
        assert!(!adds_object("select_all"));
        assert!(!adds_object("add_named"));
        assert!(is_added_object_member("active_object"));
        assert!(!is_added_object_member("scene"));
    }

    #[test]
    fn presets() {
        assert!(is_preset_path("scripts/presets/camera/1_inch.py"));
        assert!(is_preset_path("C:\\addon\\presets\\x\\a.py"));
        assert!(!is_preset_path("scripts/presets.py"));
        assert!(!is_preset_path("addon/operators/presets_menu.py"));
    }

    #[test]
    fn menu_prefixes() {
        assert_eq!(menu_space_type("CLIP_MT_view"), Some("CLIP_EDITOR"));
        assert_eq!(menu_space_type("TIME_MT_view"), Some("DOPESHEET_EDITOR"));
        assert_eq!(menu_space_type("TOPBAR_MT_file"), Some("TOPBAR"));
        assert_eq!(menu_space_type("OBJECT_MT_modifier_add"), None);
        assert_eq!(menu_space_type("MYADDON_MT_menu"), None);
        assert_eq!(menu_space_type("VIEW3D_PT_tools"), None);
    }
}
