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
//! - [`index`]: the Add-on index, what an add-on's code does to Blender, gathered without inference;
//! - [`member`]: the property behind an attribute of a Blender instance, of either origin;
//! - [`callbacks`]: the `self` that a property's callbacks are called with;
//! - [`vector`]: the value and write types of vector properties;
//! - [`lints`]: the Blender lints and the per-file query that reports them;
//! - [`rules`]: the property rules, what Blender rejects or ignores in property declarations.

mod addon;
mod callbacks;
mod evaluate;
mod follow;
mod index;
mod lints;
mod member;
mod module_dunders;
mod properties;
mod rules;
mod symbols;
mod vector;

pub(crate) use addon::addon_of_file;
pub(crate) use callbacks::declaration_callee;
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
pub(crate) use properties::may_be_property_call;
pub(crate) use symbols::hides_star_import;
