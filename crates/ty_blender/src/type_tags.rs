//! Type tags: the read-only attribute (`Modifier.type`, `Node.bl_idname`) whose value names the
//! subclass Blender refines a struct to. Which value names which class depends on the Blender
//! version and is read from the stubs; what holds for every version is here.

/// The tag Blender gives every struct an add-on defines: a Python node, socket or node tree has
/// the type `'CUSTOM'`. A stub class that declares it (`NodeSocketVirtual`) is not the only class
/// with that tag, so a test of it narrows nothing.
pub const ADDON_TYPE_TAG: &str = "CUSTOM";

/// Whether a failed test of the tag `attribute` rules out the classes that declare the tested
/// value. Not for `bl_idname`: an add-on class derived from a built-in one
/// (`ShaderNodeCustomGroup`) is an instance of it with an `bl_idname` of its own.
pub fn excludes_on_mismatch(attribute: &str) -> bool {
    attribute != "bl_idname"
}

/// The `bpy.types` classes a member is never looked up in the subclasses of: every struct is a
/// `bpy_struct` and every data-block an `ID`, so a member one of their subclasses declares says
/// nothing about which struct the code holds.
pub const NO_SUBCLASS_MEMBERS: [&str; 2] = ["bpy_struct", "ID"];

/// A collection method that creates a struct from an idname: the argument `parameter` of
/// `class.method(...)` is the `attribute` Type tag of the struct it returns. Blender types these
/// parameters as `str` (any registered type, an add-on's included), so no `rna_enum_*` alias pairs
/// them with the tag.
#[derive(Debug, Clone, Copy)]
pub struct IdnameConstructor {
    pub class: &'static str,
    pub method: &'static str,
    pub parameter: &'static str,
    pub attribute: &'static str,
}

/// The constructors whose argument is a `bl_idname`: `nodes.new(type=...)` looks the node type up
/// by idname (`rna_NodeTree_node_new`, `nodeAddNode`), `gizmos.new(type=...)` the gizmo type
/// (`rna_GizmoGroup_gizmo_new`, `WM_gizmotype_find`), and the new struct is refined to that type's
/// class, an add-on's own one included.
const IDNAME_CONSTRUCTORS: [IdnameConstructor; 2] = [
    IdnameConstructor {
        class: "Nodes",
        method: "new",
        parameter: "type",
        attribute: "bl_idname",
    },
    IdnameConstructor {
        class: "Gizmos",
        method: "new",
        parameter: "type",
        attribute: "bl_idname",
    },
];

/// The constructor `class.method` whose argument is a `bl_idname`, if it is one.
pub fn idname_constructor(class: &str, method: &str) -> Option<IdnameConstructor> {
    IDNAME_CONSTRUCTORS
        .into_iter()
        .find(|constructor| constructor.class == class && constructor.method == method)
}
