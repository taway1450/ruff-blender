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

use self::CallbackKind::{ClassMethod, Function};
use self::Presence::{Optional, Required};
use self::Returns::{Truth, Value};

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
        let mistake = self.mistake(shape)?;
        Some(format!(
            "{mistake}; Blender refuses the class when it registers it"
        ))
    }

    /// What is wrong with `shape` for this callback, or `None` when nothing is.
    fn mistake(&self, shape: CallbackShape) -> Option<String> {
        let name = self.name;
        let root = self.root;
        match (self.kind, shape.kind) {
            (CallbackKind::ClassMethod, MethodKind::Function) => {
                return Some(format!("`{root}.{name}` must be a classmethod"));
            }
            (CallbackKind::ClassMethod, MethodKind::StaticMethod) => {
                return Some(format!(
                    "`{root}.{name}` must be a classmethod, not a staticmethod"
                ));
            }
            (CallbackKind::Function, MethodKind::ClassMethod) => {
                return Some(format!("`{root}.{name}` must not be a classmethod"));
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
            "`{root}.{name}` must take {expected} positional parameters ({first} and parameters with defaults included), found {found}"
        ))
    }
}

/// Whether Blender refuses a class that does not define a callback.
#[derive(Copy, Clone)]
enum Presence {
    /// `FUNC_REGISTER`.
    Required,
    /// `FUNC_REGISTER_OPTIONAL`.
    Optional,
}

/// What Blender does with a callback's result.
#[derive(Copy, Clone)]
enum Returns {
    /// Converts it with `bool()`, so any value is accepted.
    Truth,
    /// Converts it to the RNA return type, which may fail.
    Value,
}

const fn callback(
    root: &'static str,
    name: &'static str,
    kind: CallbackKind,
    positional: usize,
    presence: Presence,
    returns: Returns,
) -> Callback {
    Callback {
        root,
        name,
        kind,
        positional: positional..=positional,
        required: matches!(presence, Presence::Required),
        bool_result: matches!(returns, Returns::Truth),
    }
}

/// Every Blender callback, by registrable root. Positional counts include `self` / `cls`.
#[rustfmt::skip]
pub const CALLBACKS: [Callback; 74] = [
    callback("Operator", "poll", ClassMethod, 2, Optional, Truth),
    callback("Operator", "execute", Function, 2, Optional, Value),
    callback("Operator", "check", Function, 2, Optional, Truth),
    callback("Operator", "invoke", Function, 3, Optional, Value),
    callback("Operator", "modal", Function, 3, Optional, Value),
    callback("Operator", "draw", Function, 2, Optional, Value),
    callback("Operator", "cancel", Function, 2, Optional, Value),
    callback("Operator", "description", ClassMethod, 3, Optional, Value),
    callback("Macro", "poll", ClassMethod, 2, Optional, Truth),
    callback("Macro", "draw", Function, 2, Optional, Value),
    callback("Panel", "poll", ClassMethod, 2, Optional, Truth),
    callback("Panel", "draw", Function, 2, Required, Value),
    callback("Panel", "draw_header", Function, 2, Optional, Value),
    callback("Panel", "draw_header_preset", Function, 2, Optional, Value),
    Callback {
        // `active_property` and the parameters after it are optional
        // (`PARM_PYFUNC_REGISTER_OPTIONAL`).
        positional: 7..=10,
        ..callback("UIList", "draw_item", Function, 7, Optional, Value)
    },
    callback("UIList", "draw_filter", Function, 3, Optional, Value),
    callback("UIList", "filter_items", Function, 4, Optional, Value),
    callback("Header", "draw", Function, 2, Required, Value),
    callback("Menu", "poll", ClassMethod, 2, Optional, Truth),
    callback("Menu", "draw", Function, 2, Required, Value),
    callback("AssetShelf", "poll", ClassMethod, 2, Optional, Truth),
    callback("AssetShelf", "asset_poll", ClassMethod, 2, Optional, Truth),
    callback("AssetShelf", "get_active_asset", ClassMethod, 1, Optional, Value),
    callback("AssetShelf", "draw_context_menu", ClassMethod, 4, Optional, Value),
    callback("FileHandler", "poll_drop", ClassMethod, 2, Optional, Truth),
    // Unlike every other `poll`, an instance method (`FUNC_REGISTER` without `FUNC_NO_SELF`).
    callback("KeyingSetInfo", "poll", Function, 2, Required, Truth),
    callback("KeyingSetInfo", "iterator", Function, 3, Required, Value),
    callback("KeyingSetInfo", "generate", Function, 4, Required, Value),
    // Required, and met by Blender's own `_bpy_types.Node.poll`.
    callback("Node", "poll", ClassMethod, 2, Required, Truth),
    callback("Node", "poll_instance", Function, 2, Optional, Truth),
    callback("Node", "update", Function, 1, Optional, Value),
    callback("Node", "insert_link", Function, 2, Optional, Value),
    callback("Node", "init", Function, 2, Optional, Value),
    callback("Node", "copy", Function, 2, Optional, Value),
    callback("Node", "free", Function, 1, Optional, Value),
    callback("Node", "draw_buttons", Function, 3, Optional, Value),
    callback("Node", "draw_buttons_ext", Function, 3, Optional, Value),
    callback("Node", "draw_label", Function, 1, Optional, Value),
    callback("NodeTree", "poll", ClassMethod, 2, Optional, Truth),
    callback("NodeTree", "update", Function, 1, Optional, Value),
    callback("NodeTree", "get_from_context", ClassMethod, 2, Optional, Value),
    callback("NodeTree", "valid_socket_type", ClassMethod, 2, Optional, Truth),
    callback("NodeSocket", "draw", Function, 5, Required, Value),
    callback("NodeSocket", "draw_color", Function, 3, Optional, Value),
    callback("NodeSocket", "draw_color_simple", ClassMethod, 1, Optional, Value),
    callback("NodeTreeInterfaceSocket", "draw", Function, 3, Optional, Value),
    callback("NodeTreeInterfaceSocket", "init_socket", Function, 4, Optional, Value),
    callback("NodeTreeInterfaceSocket", "from_socket", Function, 3, Optional, Value),
    callback("Gizmo", "draw", Function, 2, Required, Value),
    callback("Gizmo", "draw_select", Function, 3, Optional, Value),
    callback("Gizmo", "test_select", Function, 3, Optional, Value),
    callback("Gizmo", "modal", Function, 4, Optional, Value),
    callback("Gizmo", "setup", Function, 1, Optional, Value),
    callback("Gizmo", "invoke", Function, 3, Optional, Value),
    callback("Gizmo", "exit", Function, 3, Optional, Value),
    callback("Gizmo", "select_refresh", Function, 1, Optional, Value),
    callback("GizmoGroup", "poll", ClassMethod, 2, Optional, Truth),
    callback("GizmoGroup", "setup_keymap", ClassMethod, 2, Optional, Value),
    callback("GizmoGroup", "setup", Function, 2, Required, Value),
    callback("GizmoGroup", "refresh", Function, 2, Optional, Value),
    callback("GizmoGroup", "draw_prepare", Function, 2, Optional, Value),
    callback("GizmoGroup", "invoke_prepare", Function, 3, Optional, Value),
    callback("RenderEngine", "update", Function, 3, Optional, Value),
    callback("RenderEngine", "render", Function, 2, Optional, Value),
    callback("RenderEngine", "render_frame_finish", Function, 1, Optional, Value),
    callback("RenderEngine", "draw", Function, 3, Optional, Value),
    callback("RenderEngine", "bake", Function, 7, Optional, Value),
    callback("RenderEngine", "view_update", Function, 3, Optional, Value),
    callback("RenderEngine", "view_draw", Function, 3, Optional, Value),
    // Blender `main` (5.3) and later.
    callback("RenderEngine", "view_pause", Function, 2, Optional, Value),
    callback("RenderEngine", "view_resume", Function, 2, Optional, Value),
    callback("RenderEngine", "update_script_node", Function, 2, Optional, Value),
    callback("RenderEngine", "update_render_passes", Function, 3, Optional, Value),
    // Blender 5.2 and later.
    callback("RenderEngine", "update_custom_camera", Function, 2, Optional, Value),
];

/// Whether the `bpy.types` class `root` has callbacks. A class's callbacks are those of the first
/// such class in its method resolution order (`Node` for a `ShaderNode` subclass).
pub fn is_callback_root(root: &str) -> bool {
    CALLBACKS.iter().any(|callback| callback.root == root)
}

/// Whether `name` is a callback of any registrable root: a check on the name alone, before any
/// class's method resolution order is looked at.
pub fn is_callback_name(name: &str) -> bool {
    CALLBACKS.iter().any(|callback| callback.name == name)
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
