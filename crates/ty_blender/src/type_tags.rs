//! Type tags: the attribute (`Modifier.type`, `Node.bl_idname`) whose value names the subclass
//! Blender refines a struct to, or the class of a pointer below it (`Area.type`). Which value names
//! which class depends on the Blender version and is read from the stubs; what holds for every
//! version is here.
//!
//! Most tags are read-only, but not all: `Light.type`, `Texture.type` and `Area.type` can be
//! written, and Blender then refines the struct anew. So a test of a tag narrows nothing in a
//! scope that writes that tag of the same struct ([`tag_writers`]).

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
    /// The position of `parameter` among the positional arguments (`usize::MAX` for a
    /// keyword-only one).
    pub position: usize,
    pub attribute: &'static str,
    /// When the struct is not the idname's own type but one paired with it, the prefix of the
    /// idname and the one that replaces it in the name of the struct's class: `("Node",
    /// "NodeTreeInterface")` makes the interface socket of `NodeSocketFloat` a
    /// `NodeTreeInterfaceSocketFloat`.
    pub class_prefix: Option<(&'static str, &'static str)>,
}

/// The constructors whose argument is a `bl_idname`: `nodes.new(type=...)` looks the node type up
/// by idname (`rna_NodeTree_node_new`, `nodeAddNode`), a node's `inputs.new(type=...)` and
/// `outputs.new(type=...)` the socket type (`rna_Node_inputs_new`, `nodeAddSocket`),
/// `bpy.data.node_groups.new(name, type=...)` the tree type (`rna_Main_nodetree_new`,
/// `ntreeAddTree`), `gizmos.new(type=...)` the gizmo type (`rna_GizmoGroup_gizmo_new`,
/// `WM_gizmotype_find`), and the new struct is refined to that type's class, an add-on's own one
/// included. A node tree interface's `new_socket(socket_type=...)` looks the socket type up by
/// idname too (`rna_NodeTreeInterfaceItems_new_socket`), and the interface socket is refined to
/// the type's interface struct (`ext_interface.srna`), which `make_standard_socket_type` names
/// after the socket idname, `NodeTreeInterfaceSocket<X>` for `NodeSocket<X>`
/// (`node_static_socket_interface_type_new`). The call swaps a socket type the tree does not
/// support for another, but the parameter's enum lists only the supported ones
/// (`rna_NodeTreeInterfaceSocket_socket_type_itemf`, the same `is_socket_type_supported`), so an
/// unsupported one raises `TypeError` before the swap.
const IDNAME_CONSTRUCTORS: [IdnameConstructor; 6] = [
    IdnameConstructor {
        class: "Nodes",
        method: "new",
        parameter: "type",
        position: 0,
        attribute: "bl_idname",
        class_prefix: None,
    },
    IdnameConstructor {
        class: "NodeInputs",
        method: "new",
        parameter: "type",
        position: 0,
        attribute: "bl_idname",
        class_prefix: None,
    },
    IdnameConstructor {
        class: "NodeOutputs",
        method: "new",
        parameter: "type",
        position: 0,
        attribute: "bl_idname",
        class_prefix: None,
    },
    IdnameConstructor {
        class: "BlendDataNodeTrees",
        method: "new",
        parameter: "type",
        position: 1,
        attribute: "bl_idname",
        class_prefix: None,
    },
    IdnameConstructor {
        class: "NodeTreeInterface",
        method: "new_socket",
        parameter: "socket_type",
        position: usize::MAX,
        attribute: "bl_idname",
        class_prefix: Some(("Node", "NodeTreeInterface")),
    },
    IdnameConstructor {
        class: "Gizmos",
        method: "new",
        parameter: "type",
        position: 0,
        attribute: "bl_idname",
        class_prefix: None,
    },
];

/// The constructor `class.method` whose argument is a `bl_idname`, if it is one.
pub fn idname_constructor(class: &str, method: &str) -> Option<IdnameConstructor> {
    IDNAME_CONSTRUCTORS
        .into_iter()
        .find(|constructor| constructor.class == class && constructor.method == method)
}

/// The `Object.type` values of an object `bpy.data.objects.new(name, data)` creates from a `data`
/// of the `bpy.types` class `data_class` (`rna_Main_objects_new` takes the type from
/// `BKE_object_obdata_to_type`). A curve's object type is the curve's own `ob_type`, which its
/// refined class names: a `TextCurve` is a `'FONT'`, a `SurfaceCurve` a `'SURFACE'`, and a
/// `Curve` may be either or a `'CURVE'`. `None` for a class that is no object data. A `None`
/// data makes an `'EMPTY'`.
pub fn object_types_of_data(data_class: &str) -> Option<&'static [&'static str]> {
    OBJECT_DATA_TYPES
        .iter()
        .find(|(class, _)| *class == data_class)
        .map(|(_, types)| *types)
}

const OBJECT_DATA_TYPES: [(&str, &[&str]); 15] = [
    ("Mesh", &["MESH"]),
    ("TextCurve", &["FONT"]),
    ("SurfaceCurve", &["SURFACE"]),
    ("Curve", &["CURVE", "SURFACE", "FONT"]),
    ("MetaBall", &["META"]),
    ("Light", &["LIGHT"]),
    ("Speaker", &["SPEAKER"]),
    ("Camera", &["CAMERA"]),
    ("Lattice", &["LATTICE"]),
    ("Armature", &["ARMATURE"]),
    ("LightProbe", &["LIGHT_PROBE"]),
    ("Curves", &["CURVES"]),
    ("PointCloud", &["POINTCLOUD"]),
    ("Volume", &["VOLUME"]),
    ("GreasePencil", &["GREASEPENCIL"]),
];

/// A Type tag that names the class of a pointer below its struct rather than the struct's own:
/// the value of `class.attribute` is the `Space` Type tag of `<struct>.<path>`.
#[derive(Debug, Clone, Copy)]
pub struct PointerTag {
    pub class: &'static str,
    pub attribute: &'static str,
    pub path: &'static [&'static str],
}

/// The Pointer tags: an area's first space is the space of the area's type (`ED_area_newspace`
/// puts a new space first, and `Area.type` is the `spacetype` of that first space), so after
/// `area.type == 'VIEW_3D'`, `area.spaces.active` is a `SpaceView3D`.
const POINTER_TAGS: [PointerTag; 1] = [PointerTag {
    class: "Area",
    attribute: "type",
    path: &["spaces", "active"],
}];

/// The Pointer tag `class.attribute`, if it is one.
pub fn pointer_tag(class: &str, attribute: &str) -> Option<PointerTag> {
    POINTER_TAGS
        .into_iter()
        .find(|tag| tag.class == class && tag.attribute == attribute)
}

/// Whether writing the Type tag `attribute` of a `class` rebuilds the sibling pointers the tag
/// implies, so that the struct then has the members of the new value's tag view:
/// `rna_fluid_set_type` frees the fluid modifier's settings and allocates those of the new
/// `fluid_type` (`domain_settings` for `'DOMAIN'`). A tag that refines the struct's class
/// (`Light.type`) does not count: the Python object keeps the class it had.
pub fn rebuilds_on_tag_write(class: &str, attribute: &str) -> bool {
    matches!((class, attribute), ("FluidModifier", "fluid_type"))
}

/// The attributes whose write changes the Type tag `attribute`: the tag itself, and for a `type`
/// also a `ui_type` (`rna_Area_ui_type_set` sets an area's space type).
pub fn tag_writers(attribute: &str) -> impl Iterator<Item = &str> {
    std::iter::once(attribute).chain((attribute == "type").then_some("ui_type"))
}
