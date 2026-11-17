//! `UIList.draw_item`'s `item` by its source: the collection a `template_list` call shows the list
//! with.
//!
//! Blender draws a list with `layout.template_list(listtype_name, list_id, dataptr, propname, ...)`
//! and passes `draw_item` the items of the collection `dataptr.propname`, so where the list's add-on
//! names the list there, `item` has that collection's element type:
//!
//! ```python
//! class MY_UL_slots(UIList):
//!     def draw_item(self, context, layout, data, item, icon, active_data, active_propname, index):
//!         item.material  # `item` is a `MaterialSlot`
//!
//! class MY_PT_slots(Panel):
//!     def draw(self, context):
//!         ob = context.object
//!         self.layout.template_list("MY_UL_slots", "", ob, "material_slots", ob, "active_material_index")
//! ```
//!
//! The calls are looked up like the popover set ([`super::context_members::is_popover`]): in the
//! list's add-on or, for a file in no add-on (Blender's `bl_ui`), in the `.py` files of its folder,
//! through one syntax pass per file. Only the scopes of the calls that name the list are inferred,
//! for the types of their `dataptr` and `propname`. A list shown with several collections gets the
//! union of their element types. A call whose `dataptr` has no type (an unannotated parameter, as
//! `context` is in a mixin's `draw`) is left out: it says nothing, and leaving it out reports no
//! read that `bpy_struct` did not. Where a call with a typed `dataptr` gives no element type (a
//! `propname` that is no string literal or no collection of it), or no such call names the list,
//! `item` keeps the parent's type: the source is not known.
//!
//! It narrows the Assumption `blender-assumed-list-item` ([`super::assumptions`]): `item` is still
//! `None` for an empty slot of an array of ID pointers, so its reads are still reported.

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::system::SystemPath;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{self as ast, Expr};
use ty_blender::folder_source_files;
use ty_python_core::{ProgramFile, semantic_index};

use crate::types::infer::infer_complete_scope_types;
use crate::types::{StaticClassLiteral, Type, UnionType};
use crate::{Db, ProgramEnvironment};

use super::classes::class_string;
use super::index::addon_in_program;
use super::member::{attribute_value_type, without_none};
use super::type_tags::string_literals;

/// The type of `item` in the `draw_item` of the list `class`, whose parent gives it `parent` (`None`
/// excluded): the source type of the list ([`list_source_type`]) when it fits `parent`, otherwise
/// `parent`.
pub(super) fn list_item_type<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    parent: Type<'db>,
) -> Type<'db> {
    let env = ProgramEnvironment::from_file(class.program_file(db));
    list_source_type(db, class)
        .filter(|source| source.is_assignable_to(db, &env, parent))
        .unwrap_or(parent)
}

/// The union of the element types of the collections the `template_list` calls of the add-on of
/// the list `class` show it with. `None` when no call with a typed `dataptr` names the list or one
/// of them gives no element type.
#[salsa::tracked(cycle_initial=|_, _, _| None, heap_size=ruff_memory_usage::heap_size)]
fn list_source_type<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> Option<Type<'db>> {
    let file = class.program_file(db);
    let idname = class_string(db, class, "bl_idname").unwrap_or_else(|| class.name(db).to_string());
    let folder = file
        .file(db)
        .path(db)
        .as_system_path()
        .and_then(SystemPath::parent);
    let files: Vec<ProgramFile<'db>> = match (addon_in_program(db, file), folder) {
        (Some(addon), _) => addon.files(db).collect(),
        (None, Some(folder)) => folder_source_files(db, folder)
            .into_iter()
            .map(|source: File| ProgramFile::new(db, source, file.program(db)))
            .collect(),
        (None, None) => vec![file],
    };
    let env = ProgramEnvironment::from_file(file);
    let mut elements = Vec::new();
    for source in files {
        if !listed_names(db, source).contains(&Name::new(&idname)) {
            continue;
        }
        let module = parsed_module(db, source.python_file(db)).load(db);
        let mut calls = TemplateListCalls {
            name: Some(&idname),
            calls: Vec::new(),
        };
        calls.visit_body(module.suite());
        for call in calls.calls {
            match source_element_type(db, &env, source, call)? {
                SourceType::Element(element) => elements.push(element),
                SourceType::Unknown => {}
            }
        }
    }
    (!elements.is_empty()).then(|| UnionType::from_elements(db, &env, elements))
}

/// What a `template_list` call says about the items of the list it names.
enum SourceType<'db> {
    /// The element type of the collection it shows.
    Element(Type<'db>),
    /// Nothing: its `dataptr` has no type (an unannotated parameter, such as `context` in a mixin's
    /// `draw`), so it says nothing against the other calls.
    Unknown,
}

/// The element type of the collection the `template_list` call `call` in `file` shows: the type of
/// `dataptr.propname` iterated, `propname` being a string literal (or a union of them). `None` when
/// `dataptr` has a type but the collection gives no element type: a misspelt `propname`, a
/// `propname` that is no string literal, a `dataptr` whose type is too wide (`ID`).
fn source_element_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    call: &ast::ExprCall,
) -> Option<SourceType<'db>> {
    // After `*args` the positions say nothing.
    if call.arguments.args.iter().any(Expr::is_starred_expr) {
        return None;
    }
    let dataptr = call.arguments.find_argument_value("dataptr", 2)?;
    let propname = call.arguments.find_argument_value("propname", 3)?;
    let scope = semantic_index(db, file)
        .try_expression_scope_id(dataptr)?
        .to_scope_id(db, file);
    let inference = infer_complete_scope_types(db, scope);
    let data = without_none(db, inference.try_expression_type(dataptr)?);
    if data.is_dynamic() {
        return Some(SourceType::Unknown);
    }
    let names = string_literals(db, inference.try_expression_type(propname)?)?;
    let elements = names
        .iter()
        .map(|name| {
            let collection = attribute_value_type(db, env, file, data, name)?;
            let element = collection
                .try_iterate(db, env)
                .ok()?
                .homogeneous_element_type(db, env);
            (!element.is_dynamic() && !element.is_never()).then_some(element)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(SourceType::Element(UnionType::from_elements(
        db, env, elements,
    )))
}

/// The list names `file` passes as the string literal `listtype_name` of a `template_list` call, by
/// syntax alone.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn listed_names<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Box<[Name]> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    let mut calls = TemplateListCalls {
        name: None,
        calls: Vec::new(),
    };
    calls.visit_body(module.suite());
    calls
        .calls
        .into_iter()
        .filter_map(|call| listtype_name(call).map(Name::new))
        .collect()
}

/// The `template_list` calls of a module whose `listtype_name` is a string literal: all of them, or
/// those naming `name`.
struct TemplateListCalls<'a, 'b> {
    name: Option<&'b str>,
    calls: Vec<&'a ast::ExprCall>,
}

impl<'a> Visitor<'a> for TemplateListCalls<'a, '_> {
    fn visit_expr(&mut self, expression: &'a Expr) {
        if let Expr::Call(call) = expression
            && let Expr::Attribute(callee) = &*call.func
            && callee.attr.id == "template_list"
            && let Some(listed) = listtype_name(call)
            && self.name.is_none_or(|name| name == listed)
        {
            self.calls.push(call);
        }
        walk_expr(self, expression);
    }
}

/// The string literal `listtype_name` of a `template_list` call.
fn listtype_name(call: &ast::ExprCall) -> Option<&str> {
    match call.arguments.find_argument_value("listtype_name", 0)? {
        Expr::StringLiteral(literal) => Some(literal.value.to_str()),
        _ => None,
    }
}
