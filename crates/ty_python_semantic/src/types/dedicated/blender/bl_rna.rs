//! `bl_rna.properties["name"]`: the RNA property of a struct's attribute, by its kind.
//!
//! Blender refines every item of `Struct.properties` to the `Property` subclass of its kind
//! (`rna_Property_refine`): `EnumProperty`, `BoolProperty`, `IntProperty`, `FloatProperty`,
//! `StringProperty`, `PointerProperty` or `CollectionProperty`. The stubs can only say
//! `bpy_prop_collection[Property]`, since `bl_rna: Struct` is one class for every struct, so
//! `Keyframe.bl_rna.properties["interpolation"].enum_items` is an error to stock ty.
//!
//! With a string-literal key, the attribute of that name on the receiver's class tells the kind:
//!
//! ```python
//! Keyframe.bl_rna.properties["interpolation"]  # EnumProperty: `interpolation` is an enum
//! self.bl_rna.properties["count"]  # IntProperty, for `count: IntProperty()`
//! ```

use ruff_python_ast::{self as ast, Expr};

use crate::types::{KnownClass, KnownUnion, StaticClassLiteral, SubclassOfInner, Type};
use crate::{Db, ProgramEnvironment};

use super::member::without_none;
use super::overrides::is_string_literals;
use super::symbols::{bpy_types_bases, bpy_types_class_name};
use super::vector::module_class;

/// The kind of an RNA property, by the name of its `bpy.types` class.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Kind {
    Bool,
    Collection,
    Enum,
    Float,
    Int,
    Pointer,
    String,
}

impl Kind {
    fn class_name(self) -> &'static str {
        match self {
            Kind::Bool => "BoolProperty",
            Kind::Collection => "CollectionProperty",
            Kind::Enum => "EnumProperty",
            Kind::Float => "FloatProperty",
            Kind::Int => "IntProperty",
            Kind::Pointer => "PointerProperty",
            Kind::String => "StringProperty",
        }
    }
}

/// The `mathutils` classes Blender reads a float array as.
const MATH_CLASSES: [&str; 5] = ["Color", "Euler", "Matrix", "Quaternion", "Vector"];

/// The type of `subscript`, a load that ty inferred as `ty`, when it is
/// `<receiver>.bl_rna.properties["name"]` with a string-literal key, ty's item type is
/// `bpy.types.Property`, and the receiver's class (a Blender struct, its instance or a subclass)
/// has an attribute `name`: the instance of the `Property` subclass of the attribute's kind. `None`
/// keeps ty's type: a variable key, or a name the class doesn't have.
///
/// `receiver_type` gives the type ty inferred for an expression of the subscript.
pub(crate) fn rna_property_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    subscript: &ast::ExprSubscript,
    ty: Type<'db>,
    receiver_type: impl Fn(&Expr) -> Type<'db>,
) -> Option<Type<'db>> {
    let Expr::StringLiteral(key) = &*subscript.slice else {
        return None;
    };
    let Expr::Attribute(properties) = &*subscript.value else {
        return None;
    };
    if properties.attr.id.as_str() != "properties" {
        return None;
    }
    let Expr::Attribute(bl_rna) = &*properties.value else {
        return None;
    };
    if bl_rna.attr.id.as_str() != "bl_rna" {
        return None;
    }
    if !is_bpy_types_instance(db, env, ty, "Property") {
        return None;
    }
    let instance = struct_instance(db, env, receiver_type(&bl_rna.value))?;
    let attribute = instance
        .member(db, env, key.value.to_str())
        .place
        .ignore_possibly_undefined()?;
    let kind = kind_of(db, env, without_none(db, attribute))?;
    let class = module_class(db, env, "bpy.types", kind.class_name())?;
    Some(Type::instance(db, env, class.default_specialization(db)))
}

/// Whether `ty` is an instance of exactly the `bpy.types` class `name`.
fn is_bpy_types_instance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    name: &str,
) -> bool {
    let Type::NominalInstance(instance) = ty else {
        return false;
    };
    instance
        .class(db, env)
        .class_literal(db)
        .as_static()
        .is_some_and(|class| bpy_types_class_name(db, class) == Some(name))
}

/// An instance of the class of `receiver`, an instance of a Blender struct, the class itself or
/// `type[...]` of it. `None` for anything else, such as a union.
fn struct_instance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    receiver: Type<'db>,
) -> Option<Type<'db>> {
    let class: StaticClassLiteral<'db> = match receiver {
        Type::NominalInstance(instance) => {
            let class = instance.class(db, env).class_literal(db).as_static()?;
            return is_struct(db, class).then_some(receiver);
        }
        Type::ClassLiteral(class) => class.as_static()?,
        Type::SubclassOf(subclass_of) => match subclass_of.subclass_of() {
            SubclassOfInner::Class(class) => class.class_literal(db).as_static()?,
            _ => return None,
        },
        _ => return None,
    };
    is_struct(db, class).then(|| Type::instance(db, env, class.default_specialization(db)))
}

/// Whether `class` is a Blender struct: `bpy_struct` is in its method resolution order.
fn is_struct<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    bpy_types_bases(db, class).any(|base| base == "bpy_struct")
}

/// The kind of the RNA property an attribute of type `ty` (without `None`) comes from.
fn kind_of<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> Option<Kind> {
    if ty.is_dynamic() || ty.is_never() {
        return None;
    }
    if is_string_literals(db, ty) {
        return Some(Kind::Enum);
    }
    // A bool is an int, and `float` in a stub means `int | float`: test the narrowest first.
    if ty.is_assignable_to(db, env, KnownClass::Bool.to_instance(db, env)) {
        return Some(Kind::Bool);
    }
    if ty.is_assignable_to(db, env, KnownClass::Int.to_instance(db, env)) {
        return Some(Kind::Int);
    }
    if ty.is_assignable_to(db, env, KnownUnion::Float.to_type(db, env)) {
        return Some(Kind::Float);
    }
    if ty.is_assignable_to(db, env, KnownClass::Str.to_instance(db, env)) {
        return Some(Kind::String);
    }
    if let Type::Union(union) = ty {
        let mut kinds = union
            .elements(db)
            .iter()
            .map(|element| kind_of(db, env, *element));
        let first = kinds.next()??;
        return kinds.all(|kind| kind == Some(first)).then_some(first);
    }
    let (class, item) = match ty.class_specialization(db, env) {
        Some((class, specialization)) => (class, specialization.types(db).first().copied()),
        None => (
            ty.nominal_class(db, env)?.class_literal(db).as_static()?,
            None,
        ),
    };
    // An enum flag: a set of the enum's names.
    if class.is_known(db, KnownClass::Set) || class.is_known(db, KnownClass::FrozenSet) {
        return Some(Kind::Enum);
    }
    let blender_bases: Vec<&str> = bpy_types_bases(db, class).collect();
    if blender_bases.contains(&"bpy_prop_array") {
        return match item.map(|item| kind_of(db, env, item)) {
            Some(Some(kind @ (Kind::Bool | Kind::Int | Kind::Float))) => Some(kind),
            _ => None,
        };
    }
    if blender_bases.contains(&"bpy_prop_collection") {
        return Some(Kind::Collection);
    }
    if MATH_CLASSES.iter().any(|name| {
        module_class(db, env, "mathutils", name).is_some_and(|math_class| math_class == class)
    }) {
        return Some(Kind::Float);
    }
    if blender_bases.contains(&"bpy_struct") {
        return Some(Kind::Pointer);
    }
    None
}
