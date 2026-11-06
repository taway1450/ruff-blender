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
