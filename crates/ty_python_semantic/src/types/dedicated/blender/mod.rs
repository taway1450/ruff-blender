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
//! - [`walk`]: the `register()` walk, what `register()` and `unregister()` do, in order;
//! - [`index`]: the Add-on index, what an add-on's code does to Blender, gathered without inference;
//! - [`member`]: the property behind an attribute of a Blender instance, of either origin;
//! - [`mixin_self`]: `self` in a mixin's methods, typed by the `bpy.types` classes of its users;
//! - [`bl_rna`]: `bl_rna.properties["name"]` as the `Property` subclass of the attribute's kind;
//! - [`context_members`]: the Guaranteed context members, non-None inside a Blender callback, and
//!   the spaces a menu's idname prefix or a popover implies;
//! - [`poll_checks`]: what a `poll()` checked, non-None in the callbacks it gates;
//! - [`context_assumptions`]: the Assumptions about a context member that the code around the
//!   read takes for granted (an Internal operator, an editor's UI callback, a preset, an object
//!   just added);
//! - [`pointer_aliases`]: what Blender's bookkeeping, an area's type and `temp_override` say
//!   about a read, without a poll;
//! - [`type_tags`]: narrowing by a Type tag, `md.type == 'SUBSURF'` makes `md` a `SubsurfModifier`,
//!   and [`constructors`], by a constructor's argument, `constraints.new('COPY_LOCATION')`;
//! - [`subclass_members`]: a member only some of a refined base's subclasses declare
//!   (`ob.constraints[0].target`), an Assumption;
//! - [`operators`]: `bpy.ops.<category>.<name>` for an add-on's own operators;
//! - [`operator_properties`]: what an operator idname given as a string gives
//!   (`layout.operator("my_addon.run").count`), for add-on and built-in operators;
//! - [`preferences`]: `addons[KEY].preferences` as an add-on's own preferences class, and
//!   [`addon_key`], which add-on such a key or a preferences `bl_idname` names;
//! - [`callbacks`]: the `self` that a property's callbacks are called with;
//! - [`class_rules`]: the class-shape rules, what Blender does with a class's bases that its stubs
//!   don't show;
//! - [`classes`]: the classes of an add-on, their attributes as Blender reads them and which of them
//!   may be registered;
//! - [`vector`]: the value and write types of vector properties;
//! - [`lints`]: the Blender lints and the per-file query that reports them, and [`categories`],
//!   the categories of the rules that accept code and only report it;
//! - [`rules`]: the property rules, what Blender rejects or ignores in property declarations;
//! - [`imprecise`]: the Imprecise annotations, code less precise than the stubs that Blender
//!   accepts, typed as the precise form and reported;
//! - [`overrides`]: overrides of Blender callbacks, what Blender refuses, what ty's override
//!   checks compare them with and the types they take from their parent;
//! - [`registration_checks`]: the registration checks, what Blender refuses, ignores or leaves
//!   behind when it runs `register()` and `unregister()`;
//! - [`assumptions`]: the Assumptions, facts Blender keeps in ordinary use but does not guarantee,
//!   taken as given and reported, and [`list_sources`], a list's `item` typed by the collection its
//!   `template_list` calls show;
//! - [`reload_guard`]: the names a reload guard (`if "bpy" in locals():`) reads and the submodules a
//!   package imports by name.

mod addon;
mod addon_key;
mod assumptions;
mod bl_rna;
mod callbacks;
mod categories;
mod class_rules;
mod classes;
mod constructors;
mod context_assumptions;
mod context_members;
mod evaluate;
mod follow;
mod id_data;
mod imprecise;
mod index;
mod lints;
mod list_sources;
mod member;
mod mixin_self;
mod module_dunders;
mod operator_properties;
mod operators;
mod overrides;
mod pointer_aliases;
mod poll_checks;
mod preferences;
mod properties;
mod registration_checks;
mod reload_guard;
mod rules;
mod subclass_members;
mod symbols;
mod temp_override;
mod test_cases;
mod type_tags;
mod vector;
mod walk;

pub(crate) use addon::addon_of_file;
pub(crate) use bl_rna::rna_property_type;
pub(crate) use callbacks::declaration_callee;
pub use categories::BlenderRuleCategory;
pub(crate) use class_rules::has_invalid_bases;
pub(crate) use evaluate::{
    annotation_value_type, is_opaque_call, is_property_container, reference_value_type,
    subscript_value_type,
};
pub(crate) use imprecise::{
    accepts_enum_argument, check_enum_write, check_imprecise_result, return_annotation,
};
pub(crate) use lints::{file_diagnostics, register_lints};
pub(in crate::types) use member::on_store;
pub(crate) use member::{
    check_property_write, completions, exempts_attribute_override, hides_class_member,
    hides_instance_member, on_load,
};
pub(crate) use mixin_self::mixin_self_bounds;
pub(crate) use module_dunders::package_type;
pub use operator_properties::blender_idname_hover;
pub(crate) use operator_properties::{idname_completions, on_call};
pub(crate) use overrides::{
    apply_override_types, override_contract, override_parameter_type, override_return_type,
};
pub(crate) use properties::may_be_property_call;
pub(crate) use reload_guard::reload_guard_type;
pub(crate) use symbols::hides_star_import;
pub(crate) use test_cases::is_test_case_instance;
pub(crate) use type_tags::narrow_type_tag;
