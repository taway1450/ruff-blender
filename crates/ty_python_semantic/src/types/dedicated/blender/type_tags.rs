//! Narrowing by a Type tag: a test of the tag of a `bpy.types` struct tells which subclass it is.
//!
//! Blender refines a struct to the subclass its tag names. The stubs state it in two forms, and both
//! give the same table, (base, attribute, value) → class:
//!
//! ```python
//! class Modifier(bpy_struct):
//!     type: rna_enum_object_modifier_type_items
//! class SubsurfModifier(Modifier):
//!     type: Literal['SUBSURF']    # value form: (Modifier, type, 'SUBSURF') → SubsurfModifier
//!
//! class Object(ID):
//!     type: rna_enum_object_type_items
//!     data: ID | None
//! class _Object_MESH(Object):     # sibling form, a private tag view:
//!     type: Literal['MESH']       # (Object, type, 'MESH') → _Object_MESH
//!     data: Mesh
//! ```
//!
//! A test narrows the tested struct to the table's class, so `md.type == 'SUBSURF'` makes `md` a
//! `SubsurfModifier`, and `ob.type == 'MESH'` makes `ob` an `_Object_MESH`, whose `data` is a
//! `Mesh` through ordinary member lookup. `in` gives the union of the classes of its values; `!=`,
//! `not in` and the `else` branch rule them out. The table holds the stub classes only: an
//! add-on's own node has a `bl_idname` no stub class declares, and is not narrowed.
//!
//! A Pointer tag names the class of a pointer below the struct instead ([`ty_blender::pointer_tag`]):
//! `area.type == 'VIEW_3D'` makes `area` an `Area & P`, where the synthesised protocol `P` has
//! `spaces: Q` and `Q` has `active: SpaceView3D`, so `area.spaces.active` is a `SpaceView3D` through
//! ordinary member lookup.
//!
//! The narrowing is ty's own, so rebinding the struct drops it. Some tags can be written
//! (`Light.type`, `Area.type`, which `Area.ui_type` sets too): a scope that writes the tag of the
//! same place anywhere gets no narrowing from a test of it ([`ty_blender::tag_writers`]).

use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, name::Name};
use ruff_text_size::Ranged;
use ty_blender::{ADDON_TYPE_TAG, excludes_on_mismatch, pointer_tag, tag_writers};
use ty_python_core::ProgramFile;
use ty_python_core::place::{PlaceExpr, PlaceTable};
use ty_python_core::place_table;
use ty_python_core::scope::ScopeId;

use crate::place::imported_symbol;
use crate::types::{IntersectionBuilder, StaticClassLiteral, Type, UnionType};
use crate::{Db, ProgramEnvironment};

use super::classes::{class_string, maybe_registered_classes, mro_classes};
use super::context_members::space_subclass;
use super::index::addon_in_program;
use super::symbols::{bpy_types_class_name, is_bpy_types_class};

/// The type of the struct `value_ty` once the test `<value>.<attribute> <operator> <rhs>` gave
/// `is_positive`, when `attribute` is a Type tag of its `bpy.types` class. `rhs_ty` is a string
/// literal for `==` and `!=`, and a tuple of them for `in` and `not in` (a set or list display is
/// given as a tuple). `None` leaves the test to ty: a struct of no `bpy.types` class, a value no
/// stub class declares, a test that cannot rule a class out, or a `scope` that writes the tag of
/// `value`.
///
/// A `bl_idname` no stub class declares names the add-on's registered class with that `bl_idname`
/// ([`addon_class_by_idname`]): Blender wraps such a node, socket or tree in that class.
#[expect(clippy::too_many_arguments)]
pub(crate) fn narrow_type_tag<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    value: &ast::Expr,
    value_ty: Type<'db>,
    attribute: &str,
    rhs_ty: Type<'db>,
    operator: ast::CmpOp,
    is_positive: bool,
) -> Option<Type<'db>> {
    let values = match operator {
        ast::CmpOp::Eq | ast::CmpOp::NotEq => vec![string_literal(db, rhs_ty)?],
        ast::CmpOp::In | ast::CmpOp::NotIn => {
            let tuple = rhs_ty.exact_tuple_instance_spec(db)?;
            let elements = tuple.as_fixed_length()?.all_elements();
            elements
                .iter()
                .map(|element| string_literal(db, *element))
                .collect::<Option<Vec<_>>>()?
        }
        _ => return None,
    };
    // Whether the tag is one of `values` where the test narrows: the `if` branch of `==` and `in`,
    // the `else` branch of `!=` and `not in`.
    let is_one_of = matches!(operator, ast::CmpOp::Eq | ast::CmpOp::In) == is_positive;
    if writes_tag(place_table(db, scope), value, attribute) {
        return None;
    }
    if is_one_of && let Some(narrowed) = narrow_pointer_tag(db, env, value_ty, attribute, &values) {
        return Some(narrowed);
    }
    if let Some(narrowed) = narrow_by_values(db, env, value_ty, attribute, &values, is_one_of) {
        return Some(narrowed);
    }
    if !is_one_of || attribute != "bl_idname" {
        return None;
    }
    // Each value names a stub class or one of the add-on's.
    let classes = values
        .iter()
        .map(|value| {
            narrow_by_values(
                db,
                env,
                value_ty,
                attribute,
                std::slice::from_ref(value),
                true,
            )
            .or_else(|| addon_class_by_idname(db, env, scope.program_file(db), value_ty, value))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(UnionType::from_elements(db, env, classes))
}

/// The add-on's registered class whose `bl_idname` is `value`, when `value_ty` (a `Node`, a
/// `NodeSocket`, ...) is a base of it: `node.bl_idname == "MyShaderNode"` narrows a `Node` but
/// leaves a `ShaderNode` when the add-on's class is a plain `Node`. `None` when no registered class
/// or several have that `bl_idname` (a subclass that inherits it replaces its base in Blender, but
/// which one the add-on registers last is not known here).
fn addon_class_by_idname<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    value_ty: Type<'db>,
    value: &Name,
) -> Option<Type<'db>> {
    let key = addon_in_program(db, file)?;
    let mut classes = maybe_registered_classes(db, key)
        .into_iter()
        .filter(|class| class_string(db, *class, "bl_idname").as_deref() == Some(value.as_str()));
    let class = classes.next()?;
    if classes.next().is_some() {
        return None;
    }
    let class_ty = instance(db, env, class);
    class_ty
        .is_subtype_of(db, env, value_ty)
        .then_some(class_ty)
}

/// Whether the scope of `places` writes the tag `attribute` of `value` (`area.type = ...`)
/// anywhere: there a test of the tag says nothing about a later read.
fn writes_tag(places: &PlaceTable, value: &ast::Expr, attribute: &str) -> bool {
    tag_writers(attribute).any(|writer| {
        let target = ast::ExprAttribute {
            node_index: ast::AtomicNodeIndex::default(),
            range: value.range(),
            value: Box::new(value.clone()),
            attr: ast::Identifier::new(writer, value.range()),
            ctx: ast::ExprContext::Store,
        };
        PlaceExpr::try_from_expr(&target)
            .and_then(|place| places.place_id(&place))
            .is_some_and(|id| places.place(id).is_bound())
    })
}

/// The struct `value_ty` once its Pointer tag `attribute` is known to be one of `values`:
/// `Area & P`, where `P` gives the tag's pointer the union of the classes the values name.
fn narrow_pointer_tag<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value_ty: Type<'db>,
    attribute: &str,
    values: &[Name],
) -> Option<Type<'db>> {
    let positive: Vec<Type<'db>> = match value_ty {
        Type::NominalInstance(_) => vec![value_ty],
        Type::Intersection(intersection) => intersection.iter_positive(db).collect(),
        _ => return None,
    };
    let tag = positive.iter().find_map(|element| {
        let Type::NominalInstance(instance) = element else {
            return None;
        };
        let class = instance.class(db, env).class_literal(db).as_static()?;
        pointer_tag(bpy_types_class_name(db, class)?, attribute)
    })?;
    let classes = values
        .iter()
        .map(|value| space_subclass(db, env, value))
        .collect::<Option<Vec<_>>>()?;
    let pointer = tag.path.iter().rev().fold(
        UnionType::from_elements(db, env, classes),
        |member, name| Type::protocol_with_readonly_members(db, env, [(*name, member)]),
    );
    Some(
        IntersectionBuilder::new(db, env)
            .add_positive(value_ty)
            .add_positive(pointer)
            .build(),
    )
}

/// The type of the struct `value_ty` once its Type tag `attribute` is known to be one of `values`,
/// as a `poll()` that tested it implies. `None` when `attribute` is no Type tag of its class or a
/// value names no stub class.
pub(super) fn narrow_to_tag_values<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value_ty: Type<'db>,
    attribute: &str,
    values: &[Name],
) -> Option<Type<'db>> {
    narrow_by_values(db, env, value_ty, attribute, values, true)
}

/// The type of the struct `value_ty` once its Type tag `attribute` is known to be one of `values`
/// (`is_one_of`) or none of them.
fn narrow_by_values<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value_ty: Type<'db>,
    attribute: &str,
    values: &[Name],
    is_one_of: bool,
) -> Option<Type<'db>> {
    if values.iter().any(|value| value == ADDON_TYPE_TAG) {
        return None;
    }
    let (positive, negative): (Vec<Type<'db>>, Vec<Type<'db>>) = match value_ty {
        Type::NominalInstance(_) => (vec![value_ty], Vec::new()),
        Type::Intersection(intersection) => (
            intersection.iter_positive(db).collect(),
            intersection.iter_negative(db).collect(),
        ),
        _ => return None,
    };
    let tags = positive.iter().find_map(|element| {
        tags_of(db, env, *element, attribute).filter(|tags| !tags.is_empty())
    })?;
    let matching: Vec<&TypeTag<'db>> = tags
        .iter()
        .filter(|tag| values.contains(&tag.value))
        .copied()
        .collect();

    if is_one_of {
        // Every value must name a class, or the struct may be one the table does not know.
        if values
            .iter()
            .any(|value| !matching.iter().any(|tag| &tag.value == value))
        {
            return None;
        }
        let narrowed = matching.iter().map(|tag| {
            let class = instance(db, env, tag.class);
            let mut builder = IntersectionBuilder::new(db, env).add_positive(class);
            for element in &positive {
                if !class.is_subtype_of(db, env, *element) {
                    builder = builder.add_positive(*element);
                }
            }
            // A struct ruled out by an earlier test of the same tag (`elif`) is another class
            // whose tag differs: dropping it keeps `elif md.type == 'ARRAY'` an `ArrayModifier`
            // rather than `ArrayModifier & ~SubsurfModifier`.
            for element in &negative {
                if !tags_differ(db, env, class, *element, attribute) {
                    builder = builder.add_negative(*element);
                }
            }
            builder.build()
        });
        Some(UnionType::from_elements(db, env, narrowed))
    } else {
        if matching.is_empty()
            || !excludes_on_mismatch(attribute)
            || matching.iter().any(|tag| !tag.only_value)
        {
            return None;
        }
        let mut builder = IntersectionBuilder::new(db, env).add_positive(value_ty);
        for tag in matching {
            builder = builder.add_negative(instance(db, env, tag.class));
        }
        Some(builder.build())
    }
}

/// The public classes the Type tag table names for a struct of class `base`, by any of its tags,
/// in table order: the refined subclasses of a refined base. The private tag views
/// (`_Object_MESH`) only restate members of their base, and are left out.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(super) fn tagged_subclasses<'db>(
    db: &'db dyn Db,
    base: StaticClassLiteral<'db>,
) -> Box<[StaticClassLiteral<'db>]> {
    let mut classes = Vec::new();
    for tag in type_tags(db, base.program_file(db)) {
        if tag.base == base && !tag.class.name(db).starts_with('_') && !classes.contains(&tag.class)
        {
            classes.push(tag.class);
        }
    }
    classes.into_boxed_slice()
}

/// The Type tags of `base`, in table order: the attributes some of its subclasses declare as a
/// `Literal` (`Constraint.type`, `Attribute.data_type`, `Node.bl_idname`).
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(super) fn tag_attributes<'db>(db: &'db dyn Db, base: StaticClassLiteral<'db>) -> Box<[Name]> {
    let mut attributes: Vec<Name> = Vec::new();
    for tag in type_tags(db, base.program_file(db)) {
        if tag.base == base && !attributes.contains(&tag.attribute) {
            attributes.push(tag.attribute.clone());
        }
    }
    attributes.into_boxed_slice()
}

/// The value of a string literal type.
fn string_literal(db: &dyn Db, ty: Type<'_>) -> Option<Name> {
    let literal = ty.as_literal_value()?.as_string()?;
    Some(Name::new(literal.value(db)))
}

/// The string values of a type that is a string literal or a union of them.
pub(super) fn string_literals(db: &dyn Db, ty: Type<'_>) -> Option<Vec<Name>> {
    match ty {
        Type::Union(union) => union
            .elements(db)
            .iter()
            .map(|element| string_literal(db, *element))
            .collect(),
        _ => Some(vec![string_literal(db, ty)?]),
    }
}

/// The table entries of the Type tag `attribute` of the class of `ty`, when that is a `bpy.types`
/// class.
fn tags_of<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    attribute: &str,
) -> Option<Vec<&'db TypeTag<'db>>> {
    let Type::NominalInstance(instance) = ty else {
        return None;
    };
    let class = instance.class(db, env).class_literal(db).as_static()?;
    if !is_bpy_types_class(db, class) {
        return None;
    }
    Some(
        type_tags(db, class.program_file(db))
            .iter()
            .filter(|tag| tag.base == class && tag.attribute == attribute)
            .collect(),
    )
}

/// Whether instances of `class` and of `other` differ in their tag `attribute`, so that no struct
/// is both.
fn tags_differ<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: Type<'db>,
    other: Type<'db>,
    attribute: &str,
) -> bool {
    let tag = |ty: Type<'db>| {
        ty.member(db, env, attribute)
            .place
            .ignore_possibly_undefined()
    };
    if let Type::NominalInstance(_) = other
        && let (Some(tag), Some(other_tag)) = (tag(class), tag(other))
    {
        tag.is_disjoint_from(db, env, other_tag)
    } else {
        false
    }
}

pub(super) fn instance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: StaticClassLiteral<'db>,
) -> Type<'db> {
    Type::instance(db, env, class.default_specialization(db))
}

/// An entry of the Type tag table: a struct of class `base` whose `attribute` is `value` is a
/// `class`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct TypeTag<'db> {
    base: StaticClassLiteral<'db>,
    attribute: Name,
    value: Name,
    class: StaticClassLiteral<'db>,
    /// Whether `class` declares `value` alone, so that a struct whose tag is another value is not
    /// a `class`.
    only_value: bool,
}

/// The Type tag table of the `bpy.types` stub `file`: for each class `C` that declares an
/// attribute as a string `Literal` (an annotation or a property's return type) and each class `B`
/// it derives from where that attribute is wider, an entry (`B`, attribute, value) → `C` per value.
/// Every base gets the entries, so `CrossStrip` is found from `Strip` and from `EffectStrip`
/// alike; a subclass repeating its base's tag (`ActionKeyframeStrip` of `ActionStrip`) adds none.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn type_tags<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Box<[TypeTag<'db>]> {
    let env = ProgramEnvironment::from_file(file);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let mut tags = Vec::new();
    for statement in module.suite() {
        let ast::Stmt::ClassDef(class_def) = statement else {
            continue;
        };
        let attributes: Vec<&Name> = class_def
            .body
            .iter()
            .filter_map(literal_declaration)
            .collect();
        if attributes.is_empty() {
            continue;
        }
        let Some(Type::ClassLiteral(class)) =
            imported_symbol(db, &env, Some(file), class_def.name.as_str(), None)
                .place
                .ignore_possibly_undefined()
        else {
            continue;
        };
        let Some(class) = class.as_static() else {
            continue;
        };
        let class_instance = instance(db, &env, class);
        for attribute in attributes {
            let Some(tag_ty) = class_instance
                .member(db, &env, attribute)
                .place
                .ignore_possibly_undefined()
            else {
                continue;
            };
            let Some(values) = string_literals(db, tag_ty) else {
                continue;
            };
            for base in mro_classes(db, class).skip(1) {
                let Some(base_ty) = instance(db, &env, base)
                    .member(db, &env, attribute)
                    .place
                    .ignore_possibly_undefined()
                else {
                    continue;
                };
                if base_ty.is_subtype_of(db, &env, tag_ty) {
                    continue;
                }
                tags.extend(values.iter().map(|value| TypeTag {
                    base,
                    attribute: attribute.clone(),
                    value: value.clone(),
                    class,
                    only_value: values.len() == 1,
                }));
            }
        }
    }
    tags.into_boxed_slice()
}

/// The attribute a class body statement declares as a `Literal`: `type: Literal['SUBSURF']`, or a
/// property `def type(self) -> Literal['SUBSURF']`.
fn literal_declaration(statement: &ast::Stmt) -> Option<&Name> {
    let (name, annotation) = match statement {
        ast::Stmt::AnnAssign(assign) => (&assign.target.as_name_expr()?.id, &*assign.annotation),
        ast::Stmt::FunctionDef(function)
            if function.decorator_list.iter().any(|decorator| {
                decorator
                    .expression
                    .as_name_expr()
                    .is_some_and(|name| name.id == "property")
            }) =>
        {
            (&function.name.id, function.returns.as_deref()?)
        }
        _ => return None,
    };
    let ast::Expr::Subscript(subscript) = annotation else {
        return None;
    };
    let is_literal = match &*subscript.value {
        ast::Expr::Name(name) => name.id == "Literal",
        ast::Expr::Attribute(attribute) => attribute.attr.id == "Literal",
        _ => false,
    };
    is_literal.then_some(name)
}
