//! Blender add-ons: what ty knows about a module because it belongs to an add-on.
//!
//! The type-free facts (which add-on a file belongs to, its manifest and `bl_info`) come from the
//! `ty_blender` crate. This module turns them into types and diagnostics, and existing ty files call
//! into it through one-line hooks.

mod addon;
mod lints;
mod module_dunders;

pub(crate) use addon::addon_of_file;
pub(crate) use lints::{addon_diagnostics, register_lints};
pub(crate) use module_dunders::package_type;
