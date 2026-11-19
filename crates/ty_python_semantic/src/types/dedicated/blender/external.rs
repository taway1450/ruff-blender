//! Registered properties of other add-ons: the attributes another add-on, a startup package or a
//! default add-on adds to `bpy.types`.
//!
//! Blender shares one `bpy.types` between everything it loads, so once Cycles is enabled every
//! scene has `scene.cycles`. A file reads the Registered properties of its own add-on first
//! ([`super::index::registered_properties`]); a name none of them gives comes from, in order:
//!
//! 1. the add-ons its add-on declares in `blender_manifest.toml` (`[tool.blender] uses =
//!    ["cycles"]`), named as the modules Blender imports;
//! 2. the startup packages Blender always loads ([`STARTUP_PACKAGES`]);
//! 3. the add-ons Blender enables by default ([`DEFAULT_ADDONS`]). A user can disable these, so a
//!    read is reported under `blender-assumed-default-addon`.
//!
//! Each is found the way the file would import it at runtime, and read through its own `register()`
//! walk. A module the program cannot resolve, or one only stubs give, adds nothing.

use ruff_python_ast::name::Name;
use ty_blender::{DEFAULT_ADDONS, STARTUP_PACKAGES, module_addon};
use ty_module_resolver::{ModuleName, resolve_real_module_confident};
use ty_python_core::ProgramFile;

use crate::types::{ClassBase, StaticClassLiteral};
use crate::{Db, ProgramEnvironment};

use super::addon::addon_of_file;
use super::index::{AddonInProgram, RegisteredProperty, addon_in_program, addon_index};

/// Why another add-on's registrations count in a file.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) enum ExternalSource {
    /// The file's add-on declares it in its manifest.
    Declared,
    /// A startup package: Blender always loads it.
    Startup,
    /// An add-on Blender enables by default, which a user can disable.
    Default,
}

/// An add-on, or a startup package, whose registrations a file may read.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct ExternalAddon<'db> {
    /// The module Blender imports it as.
    pub(super) name: Name,
    #[get_size(ignore)]
    pub(super) key: AddonInProgram<'db>,
    pub(super) source: ExternalSource,
}

/// The add-ons whose registrations `file` may read besides its own add-on's, in the order they
/// are looked up: declared, startup, default. Its own add-on is never one of them.
#[salsa::tracked(returns(deref), heap_size=ruff_memory_usage::heap_size)]
pub(super) fn external_addons<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Box<[ExternalAddon<'db>]> {
    let own = addon_in_program(db, file);
    let declared = addon_of_file(db, file)
        .as_ref()
        .and_then(|addon| addon.manifest_file(db))
        .map(|manifest| ty_blender::manifest(db, manifest).uses.clone())
        .unwrap_or_default();
    let names = declared
        .iter()
        .map(|name| (name.as_str(), ExternalSource::Declared))
        .chain(
            STARTUP_PACKAGES
                .iter()
                .map(|name| (*name, ExternalSource::Startup)),
        )
        .chain(
            DEFAULT_ADDONS
                .iter()
                .map(|name| (*name, ExternalSource::Default)),
        );
    let mut addons: Vec<ExternalAddon<'db>> = Vec::new();
    for (name, source) in names {
        let Some(key) = resolve_addon(db, file, name) else {
            continue;
        };
        if Some(key) == own || addons.iter().any(|addon| addon.key == key) {
            continue;
        }
        addons.push(ExternalAddon {
            name: Name::new(name),
            key,
            source,
        });
    }
    addons.into_boxed_slice()
}

/// The add-on Blender imports as `name`, found as `file` would import it at runtime (stubs
/// skipped: fake-bpy-module ships `bl_operators` stubs, which have no `register()` body): the
/// first-party add-on that holds the module, or the package or file the module is.
fn resolve_addon<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    name: &str,
) -> Option<AddonInProgram<'db>> {
    let env = ProgramEnvironment::from_file(file);
    let module =
        resolve_real_module_confident(db, env.resolver_environment(db), &ModuleName::new(name)?)?;
    let module = ProgramFile::new(db, module.file(db)?, file.program(db));
    if let Some(key) = addon_in_program(db, module) {
        return Some(key);
    }
    let path = module.file(db).path(db).as_system_path()?;
    let addon = module_addon(db, name, path)?;
    Some(AddonInProgram::new(db, addon, file.program(db)))
}

/// The registration of `name` that another add-on gives instances of `class` in `file`: the last
/// one on the first class in the method resolution order that has one, from the first add-on of
/// [`external_addons`] that registers it there.
pub(super) fn external_registration<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<(&'db RegisteredProperty<'db>, &'db ExternalAddon<'db>)> {
    let addons = external_addons(db, file);
    if addons.is_empty() {
        return None;
    }
    class.iter_mro(db, None).find_map(|base| {
        let ClassBase::Class(base) = base else {
            return None;
        };
        let base = base.class_literal(db).as_static()?;
        addons.iter().find_map(|addon| {
            addon_index(db, addon.key)
                .registered_properties
                .iter()
                .rev()
                .find(|property| {
                    property.accepted && property.owner == base && property.name == name
                })
                .map(|property| (property, addon))
        })
    })
}
