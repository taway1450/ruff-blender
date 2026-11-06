//! Blender add-ons: what ty knows about a module because it belongs to an add-on.
//!
//! The type-free facts (which add-on a file belongs to, its manifest and `bl_info`) come from the
//! `ty_blender` crate. This module turns them into types and diagnostics, and existing ty files call
//! into it through one-line hooks.
//!
//! - [`properties`]: what one property declaration (`IntProperty(min=0)`) describes;
//! - [`index`]: the Add-on index, what an add-on's code does to Blender, gathered without inference;
//! - [`member`]: the property behind an attribute of a Blender instance, of either origin;
//! - [`callbacks`]: the `self` that a property's callbacks are called with;
//! - [`vector`]: the value and write types of vector properties.

mod addon;
mod callbacks;
mod index;
mod lints;
mod member;
mod module_dunders;
mod properties;
mod symbols;
mod vector;

pub(crate) use addon::addon_of_file;
pub(crate) use callbacks::declaration_callee;
pub(crate) use lints::{addon_diagnostics, register_lints};
pub(in crate::types) use member::on_store;
pub(crate) use member::{
    check_property_write, completions, exempts_attribute_override, hides_class_member,
    hides_instance_member, on_load,
};
pub(crate) use module_dunders::package_type;
pub(crate) use properties::{annotation_value_type, may_be_property_call};
