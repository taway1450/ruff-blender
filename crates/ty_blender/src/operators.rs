//! What Blender's operator calls take and return, and the two forms of an operator idname.

use crate::registration::IdnameFamily;

/// The execution contexts an operator call takes as its first positional argument
/// (`rna_enum_operator_context_items`, `rna_ui.cc`).
pub const EXECUTION_CONTEXTS: [&str; 12] = [
    "INVOKE_DEFAULT",
    "INVOKE_REGION_WIN",
    "INVOKE_REGION_CHANNELS",
    "INVOKE_REGION_PREVIEW",
    "INVOKE_AREA",
    "INVOKE_SCREEN",
    "EXEC_DEFAULT",
    "EXEC_REGION_WIN",
    "EXEC_REGION_CHANNELS",
    "EXEC_REGION_PREVIEW",
    "EXEC_AREA",
    "EXEC_SCREEN",
];

/// What an operator call returns, a set of these (`rna_enum_operator_return_items`).
pub const OPERATOR_RESULTS: [&str; 5] = [
    "RUNNING_MODAL",
    "CANCELLED",
    "FINISHED",
    "PASS_THROUGH",
    "INTERFACE",
];

/// An operator idname, split into its category and name: `object.select_all`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OperatorIdname {
    pub category: String,
    pub name: String,
}

impl OperatorIdname {
    /// The idname a class registers with: its `bl_idname` in the Python form (`category.name`),
    /// when Blender accepts it.
    pub fn from_bl_idname(idname: &str) -> Option<Self> {
        if IdnameFamily::Operator.problem(idname).is_some() {
            return None;
        }
        let (category, name) = idname.split_once('.')?;
        Some(OperatorIdname {
            category: category.to_string(),
            name: name.to_string(),
        })
    }

    /// An idname in either form, as Blender's operator lookup takes it (`WM_operatortype_find`):
    /// `category.name` or `CATEGORY_OT_name`.
    pub fn from_either(idname: &str) -> Option<Self> {
        if let Some((category, name)) = idname.split_once("_OT_") {
            return Self::from_bl_idname(&format!("{}.{name}", category.to_ascii_lowercase()));
        }
        Self::from_bl_idname(idname)
    }

    /// The Python form, `category.name`, what `idname_py()` returns.
    pub fn py_form(&self) -> String {
        format!("{}.{}", self.category, self.name)
    }

    /// The C form, `CATEGORY_OT_name`, what `idname()` returns.
    pub fn bl_form(&self) -> String {
        format!("{}_OT_{}", self.category.to_ascii_uppercase(), self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::OperatorIdname;

    #[test]
    fn forms() {
        let idname = OperatorIdname::from_bl_idname("opcall_macro.label").unwrap();
        assert_eq!(idname.bl_form(), "OPCALL_MACRO_OT_label");
        assert_eq!(idname.py_form(), "opcall_macro.label");
        assert_eq!(
            OperatorIdname::from_either("OPCALL_MACRO_OT_step"),
            OperatorIdname::from_bl_idname("opcall_macro.step")
        );
        assert!(OperatorIdname::from_bl_idname("OPCALL_MACRO_OT_step").is_none());
        assert!(OperatorIdname::from_either("not valid").is_none());
    }
}
