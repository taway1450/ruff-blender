//! `blender_manifest.toml`: the facts discovery and `blender-invalid-addon-metadata` need.

use ruff_db::Db;
use ruff_db::files::File;
use ruff_db::source::source_text;

/// The manifest file that makes a folder an extension.
pub const MANIFEST_FILE: &str = "blender_manifest.toml";

/// The keys every manifest must have (checked in this slice; deeper validation is out of scope).
pub const REQUIRED_MANIFEST_KEYS: [&str; 9] = [
    "schema_version",
    "id",
    "version",
    "name",
    "tagline",
    "maintainer",
    "type",
    "blender_version_min",
    "license",
];

#[derive(Clone, Debug, Default, PartialEq, Eq, get_size2::GetSize)]
pub struct Manifest {
    /// The TOML parse error, if the file does not parse. All other fields are empty then.
    pub parse_error: Option<String>,
    /// `id`, when it is a string.
    pub id: Option<String>,
    /// `type`, when it is a string.
    pub addon_type: Option<String>,
    /// The required keys the manifest lacks, in [`REQUIRED_MANIFEST_KEYS`] order.
    pub missing_keys: Vec<&'static str>,
}

impl Manifest {
    /// A theme is not an add-on.
    pub fn is_theme(&self) -> bool {
        self.addon_type.as_deref() == Some("theme")
    }

    /// `id` when Blender would accept it as the add-on's module name.
    pub fn valid_id(&self) -> Option<&str> {
        self.id.as_deref().filter(|id| is_identifier(id))
    }
}

#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn manifest(db: &dyn Db, file: File) -> Manifest {
    let source = source_text(db, file);
    let table = match source.as_str().parse::<toml::Table>() {
        Ok(table) => table,
        Err(error) => {
            return Manifest {
                parse_error: Some(error.to_string()),
                ..Manifest::default()
            };
        }
    };
    let string = |key: &str| {
        table
            .get(key)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    };
    Manifest {
        parse_error: None,
        id: string("id"),
        addon_type: string("type"),
        missing_keys: REQUIRED_MANIFEST_KEYS
            .into_iter()
            .filter(|key| !table.contains_key(*key))
            .collect(),
    }
}

/// The `addons` of the `[tool.blender]` table of a `pyproject.toml`, or `None` when it has no such table.
/// Entries that are not strings are dropped.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn tool_blender_addons(db: &dyn Db, file: File) -> Option<Vec<String>> {
    let source = source_text(db, file);
    let table = source.as_str().parse::<toml::Table>().ok()?;
    let blender = table.get("tool")?.as_table()?.get("blender")?.as_table()?;
    Some(
        blender
            .get("addons")
            .and_then(toml::Value::as_array)
            .map(|addons| {
                addons
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    )
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
