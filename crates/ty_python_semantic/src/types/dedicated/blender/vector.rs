//! Vector properties: what `FloatVectorProperty(subtype="TRANSLATION")` and its siblings hold.
//!
//! Blender reads an array property through `pyrna_math_object_from_array`. A float array that is
//! one-dimensional, or two-dimensional with subtype `MATRIX`, becomes a `mathutils` object chosen
//! by its subtype and its *total* length:
//!
//! | Subtype | Total length | Value |
//! | --- | --- | --- |
//! | `COORDINATES`, `TRANSLATION`, `DIRECTION`, `VELOCITY`, `ACCELERATION`, `XYZ`, `XYZ_LENGTH` | 2 to 4 | `Vector` |
//! | `MATRIX` | 9 or 16 | `Matrix` |
//! | `EULER`, `QUATERNION` | 3 / 4 | `Euler` / `Quaternion` |
//! | `COLOR`, `COLOR_GAMMA` | 3 | `Color` |
//!
//! Every other array, every Int and every Bool vector included, is a `bpy_prop_array` nested once
//! per dimension. A write accepts a sequence nested the same way, the value type itself (except a
//! `Matrix` on a one-dimensional property, which Blender rejects), and for a float vector a
//! `Vector` (one dimension) or a `Matrix` (two dimensions).

use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Expr};
use ty_module_resolver::{ModuleName, resolve_module_confident};
use ty_python_core::ProgramFile;

use crate::place::imported_symbol;
use crate::types::{KnownClass, KnownUnion, StaticClassLiteral, Type, UnionType};
use crate::{Db, ProgramEnvironment};

/// What a declaration says about an option that decides the value type: it is not written, it is
/// written with a value the checker reads, or it is written with a value the checker cannot read.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) enum Known<T> {
    Absent,
    Value(T),
    Unknown,
}

impl<T> Known<T> {
    /// The value that `read` gives, or unknown when it gives none.
    pub(super) fn from_read(value: Option<T>) -> Self {
        value.map_or(Known::Unknown, Known::Value)
    }

    /// The value, `default` when the option is absent, or `None` when it is unreadable.
    pub(super) fn or_default(self, default: T) -> Option<T> {
        match self {
            Known::Absent => Some(default),
            Known::Value(value) => Some(value),
            Known::Unknown => None,
        }
    }
}

/// The scalar type an Int, Float or Bool vector holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) enum Element {
    Bool,
    Float,
    Int,
}

impl Element {
    fn to_type<'db>(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        match self {
            Element::Bool => KnownClass::Bool.to_instance(db, env),
            Element::Int => KnownClass::Int.to_instance(db, env),
            // What the annotation `float` means, so that `bpy_prop_array[float]` matches.
            Element::Float => KnownUnion::Float.to_type(db, env),
        }
    }
}

/// The `mathutils` class that Blender reads a float vector as.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum MathClass {
    Color,
    Euler,
    Matrix,
    Quaternion,
    Vector,
}

impl MathClass {
    fn name(self) -> &'static str {
        match self {
            MathClass::Color => "Color",
            MathClass::Euler => "Euler",
            MathClass::Matrix => "Matrix",
            MathClass::Quaternion => "Quaternion",
            MathClass::Vector => "Vector",
        }
    }

    /// An instance of the class, or `Unknown` when the `mathutils` stubs don't define it.
    fn to_instance<'db>(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        module_class(db, env, "mathutils", self.name()).map_or_else(Type::unknown, |class| {
            Type::instance(db, env, class.default_specialization(db))
        })
    }
}

/// The shape of a vector property: its `size=` and, for a `FloatVectorProperty`, its `subtype=`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) struct VectorShape {
    pub(super) element: Element,
    /// The dimensions, each in `[1, 32]`, one to three of them.
    pub(super) size: Known<Box<[u8]>>,
    pub(super) subtype: Known<Name>,
}

impl VectorShape {
    /// The dimensions and the subtype, with `size=3` and `subtype="NONE"` when they are absent, or
    /// `None` when one that decides the type is unreadable. Only a float vector's subtype does.
    fn resolved(&self) -> Option<(Box<[u8]>, Name)> {
        let size = self.size.clone().or_default(Box::new([3]))?;
        let subtype = match self.element {
            Element::Float => self.subtype.clone().or_default(Name::new_static("NONE"))?,
            Element::Bool | Element::Int => Name::new_static("NONE"),
        };
        Some((size, subtype))
    }

    /// The `mathutils` class Blender reads this vector as, if any.
    fn math_class(&self, size: &[u8], subtype: &str) -> Option<MathClass> {
        if self.element != Element::Float {
            return None;
        }
        if !(size.len() == 1 || (size.len() == 2 && subtype == "MATRIX")) {
            return None;
        }
        let length: u32 = size.iter().map(|dimension| u32::from(*dimension)).product();
        match (subtype, length) {
            (
                "COORDINATES" | "TRANSLATION" | "DIRECTION" | "VELOCITY" | "ACCELERATION" | "XYZ"
                | "XYZ_LENGTH",
                2..=4,
            ) => Some(MathClass::Vector),
            ("MATRIX", 9 | 16) => Some(MathClass::Matrix),
            ("EULER" | "QUATERNION", 3) => Some(MathClass::Euler),
            ("EULER" | "QUATERNION", 4) => Some(MathClass::Quaternion),
            ("COLOR" | "COLOR_GAMMA", 3) => Some(MathClass::Color),
            _ => None,
        }
    }

    /// The type of the attribute Blender creates: a `mathutils` object, a nested `bpy_prop_array`,
    /// or `Any` when the size or subtype can't be read.
    pub(super) fn value_type<'db>(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Type<'db> {
        let Some((size, subtype)) = self.resolved() else {
            return Type::any();
        };
        if let Some(class) = self.math_class(&size, &subtype) {
            return class.to_instance(db, env);
        }
        let array = module_class(db, env, "bpy.types", "bpy_prop_array");
        size.iter().fold(self.element.to_type(db, env), |item, _| {
            array.map_or_else(Type::unknown, |array| {
                // Only a generic stub class can say what the array holds; a non-generic one is
                // used as it is.
                let array = array.apply_specialization(db, |generic_context| {
                    generic_context.specialize(db, vec![item])
                });
                Type::instance(db, env, array)
            })
        })
    }

    /// What a write to the attribute accepts: a sequence nested once per dimension, the value type
    /// (except a `Matrix` on a one-dimensional property), and for a float vector a `Vector` or a
    /// `Matrix` of the same number of dimensions.
    pub(super) fn write_type<'db>(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Type<'db> {
        let Some((size, subtype)) = self.resolved() else {
            return Type::any();
        };
        let sequence = size.iter().fold(self.element.to_type(db, env), |item, _| {
            KnownClass::Sequence.to_specialized_instance(db, env, &[item])
        });
        let mut accepted = vec![sequence];
        if !(size.len() == 1 && self.math_class(&size, &subtype) == Some(MathClass::Matrix)) {
            accepted.push(self.value_type(db, env));
        }
        if self.element == Element::Float {
            match size.len() {
                1 => accepted.push(MathClass::Vector.to_instance(db, env)),
                2 => accepted.push(MathClass::Matrix.to_instance(db, env)),
                _ => {}
            }
        }
        UnionType::from_elements(db, env, accepted)
    }
}

/// The dimensions that `size=` gives: an int, or a tuple or list of one to three ints, each in
/// `[1, 32]`. They are read from the syntax, else from the inferred type (`Literal[4]`, or a tuple of
/// int literals). Anything else, out-of-range values included, is unreadable.
pub(super) fn read_size<'db>(
    db: &'db dyn Db,
    size: &Expr,
    argument_type: impl Fn(&Expr) -> Type<'db>,
) -> Option<Box<[u8]>> {
    valid_size(&read_dimensions(db, size, argument_type)?)
}

/// The dimensions as Blender accepts them: one to three, each in `[1, 32]`.
pub(super) fn valid_size(dimensions: &[i64]) -> Option<Box<[u8]>> {
    if !(1..=3).contains(&dimensions.len()) {
        return None;
    }
    dimensions
        .iter()
        .map(|dimension| {
            u8::try_from(*dimension)
                .ok()
                .filter(|dimension| (1..=32).contains(dimension))
        })
        .collect()
}

/// The value of an int literal, `-1` included.
pub(super) fn int_literal(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::NumberLiteral(ast::ExprNumberLiteral {
            value: ast::Number::Int(int),
            ..
        }) => int.as_i64(),
        Expr::UnaryOp(ast::ExprUnaryOp {
            op: ast::UnaryOp::USub,
            operand,
            ..
        }) => int_literal(operand)?.checked_neg(),
        _ => None,
    }
}

/// The dimensions `size=` is written with, whether Blender accepts them or not: an int, or a tuple
/// or list of ints, read from the syntax, else from the inferred type.
pub(super) fn read_dimensions<'db>(
    db: &'db dyn Db,
    size: &Expr,
    argument_type: impl Fn(&Expr) -> Type<'db>,
) -> Option<Vec<i64>> {
    Some(match size {
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => elts
            .iter()
            .map(|element| int_literal(element).or_else(|| int_type(argument_type(element))))
            .collect::<Option<_>>()?,
        _ => match int_literal(size) {
            Some(int) => vec![int],
            None => {
                let ty = argument_type(size);
                match int_type(ty) {
                    Some(int) => vec![int],
                    None => ty
                        .exact_tuple_instance_spec(db)?
                        .as_fixed_length()?
                        .all_elements()
                        .iter()
                        .map(|element| int_type(*element))
                        .collect::<Option<_>>()?,
                }
            }
        },
    })
}

/// The value of an int literal type.
pub(super) fn int_type(ty: Type<'_>) -> Option<i64> {
    ty.as_literal_value()?.as_int()
}

/// The string that `subtype=` gives: a string literal in the syntax, else a string literal type.
pub(super) fn read_string<'db>(
    db: &'db dyn Db,
    expr: &Expr,
    argument_type: impl Fn(&Expr) -> Type<'db>,
) -> Option<Name> {
    if let Expr::StringLiteral(string) = expr {
        return Some(Name::new(string.value.to_str()));
    }
    let string = argument_type(expr).as_literal_value()?.as_string()?;
    Some(Name::new(string.value(db)))
}

/// The class `name` of the module `module`, as the stubs define it.
pub(super) fn module_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    module: &str,
    name: &str,
) -> Option<StaticClassLiteral<'db>> {
    let module =
        resolve_module_confident(db, env.resolver_environment(db), &ModuleName::new(module)?)?;
    let file = ProgramFile::new(db, module.file(db)?, env.program(db));
    let Type::ClassLiteral(class) = imported_symbol(db, env, Some(file), name, None)
        .place
        .ignore_possibly_undefined()?
    else {
        return None;
    };
    class.as_static()
}
