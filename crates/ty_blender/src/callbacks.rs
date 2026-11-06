//! Blender callbacks: the methods Blender calls on a registered class, and the names Blender's own
//! Python bases define.
//!
//! When it registers a class, Blender looks up each callback of the class's registrable root on the
//! class (`bpy_class_validate_recursive`, `bpy_rna.cc`), anywhere in its method resolution order,
//! and refuses the class when one has the wrong kind or the wrong number of positional parameters.
//! It then calls a callback with a tuple of positional arguments, as many as the callback has
//! parameters, so parameter names never matter.
//!
//! The table is hand-written from the RNA definitions of Blender `main` (`RNA_def_function` with
//! `FUNC_REGISTER` in `makesrna/intern/*.cc`), and identical in 4.2 and 5.2 apart from three
//! `RenderEngine` callbacks that older versions do not call.

use std::ops::RangeInclusive;

use self::CallbackKind::{ClassMethod as CM, Function as FN};

/// What a callback must be on the class.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum CallbackKind {
    /// A `classmethod`: Blender needs a bound method on class access (`FUNC_NO_SELF`).
    ClassMethod,
    /// A plain function on class access: a `def`, or a `staticmethod`, which class access unwraps.
    /// A `classmethod` fails.
    Function,
}

/// What a method is, as written: how its `def` is decorated.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub enum MethodKind {
    Function,
    ClassMethod,
    StaticMethod,
}

/// What Blender checks of a method that is a callback.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub struct CallbackShape {
    pub kind: MethodKind,
    /// The positional parameters, CPython's `co_argcount`: positional-only and
    /// positional-or-keyword parameters, `self` / `cls` and those with defaults included,
    /// `*args`, keyword-only parameters and `**kwargs` not.
    pub positional: usize,
}

/// A callback of a registrable root.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Callback {
    /// The `bpy.types` class that declares it.
    pub root: &'static str,
    pub name: &'static str,
    pub kind: CallbackKind,
    /// How many positional parameters Blender accepts, `self` / `cls` included. Only
    /// `UIList.draw_item` accepts a range: Blender passes as many arguments as it has parameters.
    pub positional: RangeInclusive<usize>,
    /// Whether Blender refuses a class that does not define it (`FUNC_REGISTER` without
    /// `_OPTIONAL`). `Node.poll` is met by Blender's own Python base.
    pub required: bool,
    /// Whether Blender converts the result with `bool()`, so any value is accepted.
    pub bool_result: bool,
}

impl Callback {
    /// Why Blender refuses `shape` for this callback, or `None` when it accepts it.
    pub fn problem(&self, shape: CallbackShape) -> Option<String> {
        let name = self.name;
        let root = self.root;
        match (self.kind, shape.kind) {
            (CallbackKind::ClassMethod, MethodKind::Function) => {
                return Some(format!(
                    "`{root}.{name}` must be a classmethod; Blender refuses the class when it registers it"
                ));
            }
            (CallbackKind::ClassMethod, MethodKind::StaticMethod) => {
                return Some(format!(
                    "`{root}.{name}` must be a classmethod, not a staticmethod; Blender refuses the class when it registers it"
                ));
            }
            (CallbackKind::Function, MethodKind::ClassMethod) => {
                return Some(format!(
                    "`{root}.{name}` must not be a classmethod; Blender refuses the class when it registers it"
                ));
            }
            _ => {}
        }
        if self.positional.contains(&shape.positional) {
            return None;
        }
        let first = match self.kind {
            CallbackKind::ClassMethod => "`cls`",
            CallbackKind::Function => "`self`",
        };
        let expected = if self.positional.start() == self.positional.end() {
            format!("{}", self.positional.start())
        } else {
            format!(
                "between {} and {}",
                self.positional.start(),
                self.positional.end()
            )
        };
        let found = shape.positional;
        Some(format!(
            "`{root}.{name}` must take {expected} positional parameters ({first} and parameters with defaults included), found {found}; Blender refuses the class when it registers it"
        ))
    }
}

const fn callback(
    root: &'static str,
    name: &'static str,
    kind: CallbackKind,
    positional: usize,
    required: bool,
    bool_result: bool,
) -> Callback {
    Callback {
        root,
        name,
        kind,
        positional: positional..=positional,
        required,
        bool_result,
    }
}

/// Whether a callback is required, and whether its result goes through `bool()`.
const REQ: bool = true;
const OPT: bool = false;
const BOOL: bool = true;
const ANY: bool = false;

/// Every Blender callback, by registrable root. Positional counts include `self` / `cls`.
pub const CALLBACKS: [Callback; 74] = [
    callback("Operator", "poll", CM, 2, OPT, BOOL),
    callback("Operator", "execute", FN, 2, OPT, ANY),
    callback("Operator", "check", FN, 2, OPT, BOOL),
    callback("Operator", "invoke", FN, 3, OPT, ANY),
    callback("Operator", "modal", FN, 3, OPT, ANY),
    callback("Operator", "draw", FN, 2, OPT, ANY),
    callback("Operator", "cancel", FN, 2, OPT, ANY),
    callback("Operator", "description", CM, 3, OPT, ANY),
    callback("Macro", "poll", CM, 2, OPT, BOOL),
    callback("Macro", "draw", FN, 2, OPT, ANY),
    callback("Panel", "poll", CM, 2, OPT, BOOL),
    callback("Panel", "draw", FN, 2, REQ, ANY),
    callback("Panel", "draw_header", FN, 2, OPT, ANY),
    callback("Panel", "draw_header_preset", FN, 2, OPT, ANY),
    Callback {
        root: "UIList",
        name: "draw_item",
        kind: FN,
        // `active_property` and the parameters after it are optional
        // (`PARM_PYFUNC_REGISTER_OPTIONAL`).
        positional: 7..=10,
        required: OPT,
        bool_result: ANY,
    },
    callback("UIList", "draw_filter", FN, 3, OPT, ANY),
    callback("UIList", "filter_items", FN, 4, OPT, ANY),
    callback("Header", "draw", FN, 2, REQ, ANY),
    callback("Menu", "poll", CM, 2, OPT, BOOL),
    callback("Menu", "draw", FN, 2, REQ, ANY),
    callback("AssetShelf", "poll", CM, 2, OPT, BOOL),
    callback("AssetShelf", "asset_poll", CM, 2, OPT, BOOL),
    callback("AssetShelf", "get_active_asset", CM, 1, OPT, ANY),
    callback("AssetShelf", "draw_context_menu", CM, 4, OPT, ANY),
    callback("FileHandler", "poll_drop", CM, 2, OPT, BOOL),
    // Unlike every other `poll`, an instance method (`FUNC_REGISTER` without `FUNC_NO_SELF`).
    callback("KeyingSetInfo", "poll", FN, 2, REQ, BOOL),
    callback("KeyingSetInfo", "iterator", FN, 3, REQ, ANY),
    callback("KeyingSetInfo", "generate", FN, 4, REQ, ANY),
    // Required, and met by Blender's own `_bpy_types.Node.poll`.
    callback("Node", "poll", CM, 2, REQ, BOOL),
    callback("Node", "poll_instance", FN, 2, OPT, BOOL),
    callback("Node", "update", FN, 1, OPT, ANY),
    callback("Node", "insert_link", FN, 2, OPT, ANY),
    callback("Node", "init", FN, 2, OPT, ANY),
    callback("Node", "copy", FN, 2, OPT, ANY),
    callback("Node", "free", FN, 1, OPT, ANY),
    callback("Node", "draw_buttons", FN, 3, OPT, ANY),
    callback("Node", "draw_buttons_ext", FN, 3, OPT, ANY),
    callback("Node", "draw_label", FN, 1, OPT, ANY),
    callback("NodeTree", "poll", CM, 2, OPT, BOOL),
    callback("NodeTree", "update", FN, 1, OPT, ANY),
    callback("NodeTree", "get_from_context", CM, 2, OPT, ANY),
    callback("NodeTree", "valid_socket_type", CM, 2, OPT, BOOL),
    callback("NodeSocket", "draw", FN, 5, REQ, ANY),
    callback("NodeSocket", "draw_color", FN, 3, OPT, ANY),
    callback("NodeSocket", "draw_color_simple", CM, 1, OPT, ANY),
    callback("NodeTreeInterfaceSocket", "draw", FN, 3, OPT, ANY),
    callback("NodeTreeInterfaceSocket", "init_socket", FN, 4, OPT, ANY),
    callback("NodeTreeInterfaceSocket", "from_socket", FN, 3, OPT, ANY),
    callback("Gizmo", "draw", FN, 2, REQ, ANY),
    callback("Gizmo", "draw_select", FN, 3, OPT, ANY),
    callback("Gizmo", "test_select", FN, 3, OPT, ANY),
    callback("Gizmo", "modal", FN, 4, OPT, ANY),
    callback("Gizmo", "setup", FN, 1, OPT, ANY),
    callback("Gizmo", "invoke", FN, 3, OPT, ANY),
    callback("Gizmo", "exit", FN, 3, OPT, ANY),
    callback("Gizmo", "select_refresh", FN, 1, OPT, ANY),
    callback("GizmoGroup", "poll", CM, 2, OPT, BOOL),
    callback("GizmoGroup", "setup_keymap", CM, 2, OPT, ANY),
    callback("GizmoGroup", "setup", FN, 2, REQ, ANY),
    callback("GizmoGroup", "refresh", FN, 2, OPT, ANY),
    callback("GizmoGroup", "draw_prepare", FN, 2, OPT, ANY),
    callback("GizmoGroup", "invoke_prepare", FN, 3, OPT, ANY),
    callback("RenderEngine", "update", FN, 3, OPT, ANY),
    callback("RenderEngine", "render", FN, 2, OPT, ANY),
    callback("RenderEngine", "render_frame_finish", FN, 1, OPT, ANY),
    callback("RenderEngine", "draw", FN, 3, OPT, ANY),
    callback("RenderEngine", "bake", FN, 7, OPT, ANY),
    callback("RenderEngine", "view_update", FN, 3, OPT, ANY),
    callback("RenderEngine", "view_draw", FN, 3, OPT, ANY),
    // Blender `main` (5.3) and later.
    callback("RenderEngine", "view_pause", FN, 2, OPT, ANY),
    callback("RenderEngine", "view_resume", FN, 2, OPT, ANY),
    callback("RenderEngine", "update_script_node", FN, 2, OPT, ANY),
    callback("RenderEngine", "update_render_passes", FN, 3, OPT, ANY),
    // Blender 5.2 and later.
    callback("RenderEngine", "update_custom_camera", FN, 2, OPT, ANY),
];

/// Whether the `bpy.types` class `root` has callbacks. A class's callbacks are those of the first
/// such class in its method resolution order (`Node` for a `ShaderNode` subclass).
pub fn is_callback_root(root: &str) -> bool {
    CALLBACKS.iter().any(|callback| callback.root == root)
}

/// The callbacks of the root `root`.
pub fn callbacks_of(root: &str) -> impl Iterator<Item = &'static Callback> {
    CALLBACKS
        .iter()
        .filter(move |callback| callback.root == root)
}

/// The callback `name` of the root `root`.
pub fn callback_of(root: &str, name: &str) -> Option<&'static Callback> {
    callbacks_of(root).find(|callback| callback.name == name)
}

/// Why Blender refuses a method `name` of the shape `shape` on a class with the root `root`, or
/// `None` when it accepts it or `name` is no callback of `root`.
pub fn problem(root: &str, name: &str, shape: CallbackShape) -> Option<String> {
    callback_of(root, name)?.problem(shape)
}

/// The names that Blender's Python bases define (`scripts/modules/_bpy_types.py`), keyed by the
/// `bpy.types` class that has them in its method resolution order. `Panel`, `UIList`, `Header` and
/// `Menu` share `_GenericUI`'s.
const GENERIC_UI: &[&str] = &[
    "is_extended",
    "append",
    "prepend",
    "remove",
    "_dyn_ui_initialize",
    "_dyn_owner_apply",
];

pub const PYTHON_BASE_NAMES: [(&str, &[&str]); 14] = [
    (
        "Operator",
        &[
            "as_keywords",
            "__getattribute__",
            "__setattr__",
            "__delattr__",
        ],
    ),
    ("Macro", &["define"]),
    ("Panel", GENERIC_UI),
    ("UIList", GENERIC_UI),
    ("Header", GENERIC_UI),
    (
        "Menu",
        &[
            "is_extended",
            "append",
            "prepend",
            "remove",
            "_dyn_ui_initialize",
            "_dyn_owner_apply",
            "path_menu",
            "draw_preset",
            "draw_collapsible",
        ],
    ),
    // `poll(cls, ntree)`, also an RNA callback.
    ("Node", &["poll"]),
    ("CompositorNode", &["poll"]),
    ("ShaderNode", &["poll"]),
    ("TextureNode", &["poll"]),
    ("GeometryNode", &["poll"]),
    ("NodeSocket", &["links"]),
    (
        "Gizmo",
        &[
            "draw_custom_shape",
            "new_custom_shape",
            "__getattribute__",
            "__setattr__",
            "__delattr__",
        ],
    ),
    (
        "HydraRenderEngine",
        &[
            "__init__",
            "__del__",
            "get_render_settings",
            "update",
            "render",
            "view_update",
            "view_draw",
            "bl_use_shading_nodes_custom",
            "bl_delegate_id",
        ],
    ),
];

/// The names Blender's Python base defines for the `bpy.types` class `class`; none when it has no
/// Python base or its Python base adds nothing.
pub fn python_base_names(class: &str) -> &'static [&'static str] {
    PYTHON_BASE_NAMES
        .iter()
        .find(|(name, _)| *name == class)
        .map_or(&[], |(_, names)| names)
}

#[cfg(test)]
mod tests {
    use super::{CALLBACKS, CallbackShape, MethodKind, callback_of, is_callback_root, problem};

    fn shape(kind: MethodKind, positional: usize) -> CallbackShape {
        CallbackShape { kind, positional }
    }

    #[test]
    fn table() {
        assert_eq!(CALLBACKS.iter().filter(|c| c.required).count(), 10);
        assert_eq!(CALLBACKS.iter().filter(|c| c.bool_result).count(), 14);
        for (index, callback) in CALLBACKS.iter().enumerate() {
            assert!(
                CALLBACKS[..index]
                    .iter()
                    .all(|other| (other.root, other.name) != (callback.root, callback.name)),
                "{}.{}",
                callback.root,
                callback.name
            );
        }
        assert!(is_callback_root("Operator"));
        assert!(!is_callback_root("PropertyGroup"));
        assert!(callback_of("AddonPreferences", "draw").is_none());
    }

    #[test]
    fn problems() {
        use MethodKind::{ClassMethod, Function, StaticMethod};
        assert!(problem("Operator", "execute", shape(Function, 2)).is_none());
        assert!(problem("Operator", "execute", shape(StaticMethod, 2)).is_none());
        assert!(problem("Operator", "execute", shape(Function, 1)).is_some());
        assert!(problem("Operator", "execute", shape(Function, 3)).is_some());
        assert!(problem("Operator", "execute", shape(ClassMethod, 2)).is_some());
        assert!(problem("Operator", "poll", shape(ClassMethod, 2)).is_none());
        assert!(problem("Operator", "poll", shape(Function, 2)).is_some());
        assert!(problem("Operator", "poll", shape(StaticMethod, 1)).is_some());
        assert!(problem("KeyingSetInfo", "poll", shape(Function, 2)).is_none());
        assert!(problem("UIList", "draw_item", shape(Function, 6)).is_some());
        assert!(problem("UIList", "draw_item", shape(Function, 7)).is_none());
        assert!(problem("UIList", "draw_item", shape(Function, 10)).is_none());
        assert!(problem("UIList", "draw_item", shape(Function, 11)).is_some());
        assert!(problem("Operator", "as_keywords", shape(Function, 9)).is_none());
    }
}
