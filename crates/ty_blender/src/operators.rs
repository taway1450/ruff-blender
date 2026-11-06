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

/// What a sink gives for a known idname.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SinkResult {
    /// The properties, never `None`.
    Properties,
    /// The stub's return type with `K` (a key-map item, a macro step).
    Holder,
    /// The stub's return type (`None`).
    Unchanged,
}

/// A Blender function that takes an operator idname: a method of a `bpy.types` class (`UILayout.operator`).
#[derive(Copy, Clone, Debug)]
pub struct Sink {
    /// The `bpy.types` class that defines it.
    pub class: &'static str,
    pub method: &'static str,
    /// The name of its idname parameter (always the first).
    pub parameter: &'static str,
    pub result: SinkResult,
    /// Whether its second argument, `property`, names an enum property of the operator.
    pub enum_property: bool,
}

pub const SINKS: [Sink; 9] = [
    Sink::new("UILayout", "operator", SinkResult::Properties),
    Sink::new("UILayout", "operator_menu_hold", SinkResult::Properties),
    Sink::new("UILayout", "operator_menu_enum", SinkResult::Properties).with_enum_property(),
    Sink::new("UILayout", "operator_enum", SinkResult::Unchanged).with_enum_property(),
    Sink::new(
        "WindowManager",
        "operator_properties_last",
        SinkResult::Properties,
    ),
    Sink::new("Gizmo", "target_set_operator", SinkResult::Properties),
    Sink::new(
        "WorkSpaceTool",
        "operator_properties",
        SinkResult::Properties,
    ),
    Sink {
        parameter: "idname",
        ..Sink::new("KeyMapItems", "new", SinkResult::Holder)
    },
    Sink::new("Macro", "define", SinkResult::Holder),
];

impl Sink {
    const fn new(class: &'static str, method: &'static str, result: SinkResult) -> Self {
        Sink {
            class,
            method,
            parameter: "operator",
            result,
            enum_property: false,
        }
    }

    const fn with_enum_property(self) -> Self {
        Sink {
            enum_property: true,
            ..self
        }
    }
}

/// The longest operator idname in the Python form that Blender converts (`OP_MAX_PY_IDNAME`).
const MAX_PY_IDNAME: usize = 60;

/// An operator idname, split into its category and name as Blender splits it for Python
/// (`WM_operator_py_idname`): `object.select_all`. Two idnames name the same operator exactly when
/// they are equal.
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
        Self::from_lookup(idname)
    }

    /// A macro step's idname, in either form (`_bpy.ops.macro_define`): converted as the lookup
    /// converts it, then refused unless it is a valid `CATEGORY_OT_name` idname.
    pub fn from_either(idname: &str) -> Option<Self> {
        if !bl_idname_is_valid(&bl_idname(idname)) {
            return None;
        }
        Self::from_lookup(idname)
    }

    /// The operator a string names in Blender's operator lookup (`WM_operatortype_find`). The
    /// lookup converts the string to the `CATEGORY_OT_name` form (`WM_operator_bl_idname`) and
    /// matches it exactly. Every operator's idname has an upper-case category in that form, so
    /// `"object.select_all"` and `"OBJECT_OT_select_all"` name the same operator, and
    /// `"object_OT_select_all"` none.
    pub fn from_lookup(idname: &str) -> Option<Self> {
        let bl_form = bl_idname(idname);
        let (category, name) = bl_form.split_once("_OT_")?;
        if category.is_empty()
            || name.is_empty()
            || category.bytes().any(|b| b.is_ascii_lowercase())
        {
            return None;
        }
        let py_form = py_idname(&bl_form);
        let (category, name) = py_form.split_once('.')?;
        Some(OperatorIdname {
            category: category.to_string(),
            name: name.to_string(),
        })
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

/// `WM_operator_bl_idname`: `category.name` to `CATEGORY_OT_name`, splitting at the first `.`.
/// Any other string, and one too long, stays as it is.
fn bl_idname(idname: &str) -> String {
    match idname.split_once('.') {
        Some((category, name)) if idname.len() <= MAX_PY_IDNAME => {
            format!("{}_OT_{name}", category.to_ascii_uppercase())
        }
        _ => idname.to_string(),
    }
}

/// `WM_operator_py_idname`: `CATEGORY_OT_name` to `category.name`, splitting at the first `_OT_`.
/// Any other string stays as it is.
fn py_idname(idname: &str) -> String {
    match idname.split_once("_OT_") {
        Some((category, name)) => format!("{}.{name}", category.to_ascii_lowercase()),
        None => idname.to_string(),
    }
}

/// `WM_operator_bl_idname_is_valid`: an upper-case category and a lower-case name around `_OT_`.
fn bl_idname_is_valid(idname: &str) -> bool {
    let Some((category, name)) = idname.split_once("_OT_") else {
        return false;
    };
    !category.is_empty()
        && !name.is_empty()
        && category
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
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

    #[test]
    fn lookup() {
        let select_all = OperatorIdname::from_bl_idname("object.select_all");
        assert_eq!(OperatorIdname::from_lookup("object.select_all"), select_all);
        assert_eq!(OperatorIdname::from_lookup("Object.select_all"), select_all);
        assert_eq!(
            OperatorIdname::from_lookup("OBJECT_OT_select_all"),
            select_all
        );
        assert!(OperatorIdname::from_lookup("object_OT_select_all").is_none());
        assert!(OperatorIdname::from_lookup("select_all").is_none());
    }

    #[test]
    fn first_separator() {
        // Blender registers `odd_ot_x.run` as `ODD_OT_X_OT_run`, which it splits after the first
        // `_OT_` again.
        let odd = OperatorIdname::from_bl_idname("odd_ot_x.run").unwrap();
        assert_eq!(odd.py_form(), "odd.X_OT_run");
        assert_eq!(odd.bl_form(), "ODD_OT_X_OT_run");
        assert_eq!(
            OperatorIdname::from_lookup("odd_ot_x.run"),
            Some(odd.clone())
        );
        assert_eq!(OperatorIdname::from_lookup("ODD_OT_X_OT_run"), Some(odd));
    }
}
