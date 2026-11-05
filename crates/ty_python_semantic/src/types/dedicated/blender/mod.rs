//! PROTOTYPE (wayfinder ticket #9, throwaway): Blender support glue.
//!
//! Only the use-site lookup of registered properties lives here. Discovery and collection are
//! deliberately cheap stand-ins for the designs in tickets #3 and #8.

pub(crate) mod registered;

pub(crate) use registered::{completions, on_load, on_store};
