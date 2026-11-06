//! Blender add-ons: what ty knows about a module because it belongs to an add-on.
//!
//! The type-free facts (which add-on a file belongs to, its manifest and `bl_info`) come from the
//! `ty_blender` crate. This module turns them into types and diagnostics, and existing ty files call
//! into it through one-line hooks.
//!
//! - [`properties`]: what one property declaration (`IntProperty(min=0)`) describes;
//! - [`evaluate`]: the Property evaluator, which declarations an expression evaluates to
//!   (`x: COMMON`, `x: wrapper(2)`);
//! - [`follow`]: the shared follower, what a name refers to by its definitions;
//! - [`walk`]: the `register()` walk, what `register()` and `unregister()` do, in order;
//! - [`index`]: the Add-on index, what an add-on's code does to Blender, gathered without inference;
//! - [`member`]: the property behind an attribute of a Blender instance, of either origin;
//! - [`operators`]: `bpy.ops.<category>.<name>` for an add-on's own operators;
//! - [`operator_properties`]: what an operator idname given as a string gives
//!   (`layout.operator("my_addon.run").count`), for add-on and built-in operators;
//! - [`preferences`]: `addons[KEY].preferences` as an add-on's own preferences class, and
//!   [`addon_key`], which add-on such a key or a preferences `bl_idname` names;
//! - [`callbacks`]: the `self` that a property's callbacks are called with;
//! - [`class_rules`]: the class-shape rules, what Blender does with a class's bases that its stubs
//!   don't show;
//! - [`classes`]: the classes of an add-on, their attributes as Blender reads them and which of them
//!   may be registered;
//! - [`vector`]: the value and write types of vector properties;
//! - [`lints`]: the Blender lints and the per-file query that reports them;
//! - [`rules`]: the property rules, what Blender rejects or ignores in property declarations;
//! - [`overrides`]: overrides of Blender callbacks, what Blender refuses, what ty's override
//!   checks compare them with and the types they take from their parent;
//! - [`registration_checks`]: the registration checks, what Blender refuses, ignores or leaves
//!   behind when it runs `register()` and `unregister()`;
//! - [`reload_guard`]: the names an add-on's reload guard (`if "bpy" in locals():`) uses.

mod addon;
mod addon_key;
mod callbacks;
mod class_rules;
mod classes;
mod evaluate;
mod follow;
mod index;
mod lints;
mod member;
mod module_dunders;
mod operator_properties;
mod operators;
mod overrides;
mod preferences;
mod properties;
mod registration_checks;
mod reload_guard;
mod rules;
mod symbols;
mod vector;
mod walk;

pub(crate) use addon::addon_of_file;
pub(crate) use callbacks::declaration_callee;
pub(crate) use class_rules::has_invalid_bases;
pub(crate) use evaluate::{
    annotation_value_type, is_opaque_call, is_property_container, reference_value_type,
    subscript_value_type,
};
pub(crate) use lints::{file_diagnostics, register_lints};
pub(in crate::types) use member::on_store;
pub(crate) use member::{
    check_property_write, completions, exempts_attribute_override, hides_class_member,
    hides_instance_member, on_load,
};
pub(crate) use module_dunders::package_type;
pub use operator_properties::blender_idname_hover;
pub(crate) use operator_properties::{idname_completions, on_call};
pub(crate) use overrides::{
    apply_override_types, override_contract, override_parameter_type, override_return_type,
};
pub(crate) use properties::may_be_property_call;
pub(crate) use reload_guard::reload_guard_type;
pub(crate) use symbols::hides_star_import;
