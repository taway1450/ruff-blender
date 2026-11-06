//! What Blender accepts when it registers a class: which classes `register_class` takes, and which
//! idnames it refuses.

/// The `bpy.types` classes whose subclasses `bpy.utils.register_class` accepts: the RNA structs with
/// register functions (`RNA_def_struct_register_funcs`). Their subclasses (`ShaderNode` of `Node`)
/// are accepted too.
pub const REGISTRABLE_ROOTS: [&str; 20] = [
    "PropertyGroup",
    "Operator",
    "Macro",
    "Panel",
    "Menu",
    "Header",
    "UIList",
    "AddonPreferences",
    "KeyConfigPreferences",
    "KeyingSetInfo",
    "RenderEngine",
    "AssetShelf",
    "FileHandler",
    "Gizmo",
    "GizmoGroup",
    "Node",
    "NodeTree",
    "NodeSocket",
    "NodeTreeInterfaceSocket",
    "USDHook",
];

/// The idname families: classes of one family share one table of idnames, where a second class with
/// an idname replaces the first.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum IdnameFamily {
    /// `Operator` and `Macro`: `category.name`.
    Operator,
    /// `Panel`, `Menu`, `Header` and `UIList`: `PREFIX_XX_suffix`, where the class name is the
    /// idname when the class sets none.
    Struct(&'static str),
}

impl IdnameFamily {
    /// The family of a class with the `bpy.types` base `root`.
    pub fn of_root(root: &str) -> Option<Self> {
        Some(match root {
            "Operator" | "Macro" => IdnameFamily::Operator,
            "Panel" => IdnameFamily::Struct("_PT_"),
            "Menu" => IdnameFamily::Struct("_MT_"),
            "Header" => IdnameFamily::Struct("_HT_"),
            "UIList" => IdnameFamily::Struct("_UL_"),
            _ => return None,
        })
    }

    /// Why Blender refuses `idname` for a class of this family, or `None` when it accepts it.
    pub fn problem(self, idname: &str) -> Option<String> {
        match self {
            IdnameFamily::Operator => operator_idname_problem(idname),
            IdnameFamily::Struct(separator) => struct_idname_problem(idname, separator),
        }
    }
}

/// The longest operator idname Blender accepts in its Python form (`OP_MAX_PY_IDNAME`).
const OPERATOR_IDNAME_MAX: usize = 60;

/// Why Blender refuses the operator idname `idname`, following
/// `operator_idname_ok_or_report_impl` (`wm_operators.cc`): lower-case letters, digits and `_`, with
/// exactly one `.` that is neither first nor last, and at most 60 characters.
fn operator_idname_problem(idname: &str) -> Option<String> {
    if idname.is_empty() {
        return Some("an operator idname must be `category.name`, not empty".to_string());
    }
    for (position, character) in idname.chars().enumerate() {
        let valid = character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '_'
            || (character == '.' && position != 0 && position + 1 != idname.chars().count());
        if !valid {
            return Some(format!(
                "`{idname}` has an invalid character at position {position}; an operator idname is `category.name` in lower-case letters, digits and `_`"
            ));
        }
    }
    if idname.chars().count() > OPERATOR_IDNAME_MAX {
        return Some(format!(
            "`{idname}` is longer than {OPERATOR_IDNAME_MAX} characters"
        ));
    }
    if idname.matches('.').count() != 1 {
        return Some(format!(
            "`{idname}` must contain exactly one `.`, as in `category.name`"
        ));
    }
    None
}

/// Why Blender warns about the panel, menu, header or UI list idname `idname`, following
/// `RNA_struct_bl_idname_ok_or_report` (`rna_access.cc`): an upper-case alphanumeric prefix,
/// `separator` (`_PT_`), then an alphanumeric suffix.
fn struct_idname_problem(idname: &str, separator: &str) -> Option<String> {
    let missing = || {
        Some(format!(
            "`{idname}` does not contain `{separator}` with a prefix and a suffix, as in `MY{separator}name`"
        ))
    };
    let Some(position) = idname.find(separator) else {
        return missing();
    };
    let prefix = &idname[..position];
    let suffix = &idname[position + separator.len()..];
    if prefix.is_empty() || suffix.is_empty() {
        return missing();
    }
    let prefix_ok = prefix.char_indices().all(|(index, character)| {
        character.is_ascii_uppercase()
            || (index != 0 && character.is_ascii_digit())
            || (index != 0 && index + 1 != prefix.len() && character == '_')
    });
    if !prefix_ok {
        return Some(format!(
            "`{idname}` does not have an upper-case alphanumeric prefix before `{separator}`"
        ));
    }
    let suffix_ok = suffix.char_indices().all(|(index, character)| {
        character.is_ascii_alphanumeric()
            || (index != 0 && index + 1 != suffix.len() && character == '_')
    });
    if !suffix_ok {
        return Some(format!(
            "`{idname}` does not have an alphanumeric suffix after `{separator}`"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{operator_idname_problem, struct_idname_problem};

    #[test]
    fn operator_idnames() {
        assert!(operator_idname_problem("my_addon.run_2").is_none());
        for invalid in [
            "",
            "nocategory",
            ".name",
            "category.",
            "a.b.c",
            "has-dash.x",
            "a.Upper",
            "UPPER_CASE",
        ] {
            assert!(operator_idname_problem(invalid).is_some(), "{invalid}");
        }
    }

    #[test]
    fn struct_idnames() {
        assert!(struct_idname_problem("MY_ADDON_PT_main", "_PT_").is_none());
        assert!(struct_idname_problem("RegchkIdnamePanel", "_PT_").is_some());
        assert!(struct_idname_problem("my_PT_x", "_PT_").is_some());
        assert!(struct_idname_problem("MY_PT_", "_PT_").is_some());
        assert!(struct_idname_problem("regchk_idname.menu", "_MT_").is_some());
    }
}
