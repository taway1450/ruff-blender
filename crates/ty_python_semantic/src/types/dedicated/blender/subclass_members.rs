//! The subclass member Assumption, `blender-assumed-subclass-member`: a member that a refined base
//! lacks but some of the subclasses its Type tag names declare.
//!
//! A struct read by index or by name is typed as its refined base, though Blender always hands it
//! out as one of the subclasses its tag names:
//!
//! ```python
//! con = ob.constraints[0]     # `Constraint`
//! con.target                  # declared by 30 subclasses: `Object | None`, reported
//! con.targt                   # declared by none: still an error
//! ```
//!
//! The read is typed as the union of the subclasses' declarations and reported. Only a `bpy.types`
//! class with a Type tag counts as a refined base, never `bpy_struct` or `ID`
//! ([`ty_blender::NO_SUBCLASS_MEMBERS`]): their subclasses are every struct and every data-block.
//! An add-on's own class is not one either, so a misspelt member of an add-on's node stays an
//! error.

use ruff_python_ast as ast;
use ty_blender::NO_SUBCLASS_MEMBERS;

use crate::place::{Place, PlaceAndQualifiers};
use crate::types::context::InferContext;
use crate::types::{StaticClassLiteral, Type, UnionType};
use crate::{Db, ProgramEnvironment};

use super::lints::BLENDER_ASSUMED_SUBCLASS_MEMBER;
use super::symbols::bpy_types_class_name;
use super::type_tags::{instance, tagged_subclasses};

/// How many of the declaring subclasses the message names.
const NAMED_SUBCLASSES: usize = 3;

/// The member `attribute` read from `receiver`, where ty found nothing: the union of the
/// declarations of the subclasses that declare it, reported, when `receiver` is an instance of a
/// refined base. `None` leaves the read as it is.
pub(super) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
    place: PlaceAndQualifiers<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    if !attribute.ctx.is_load() || !place.is_undefined() {
        return None;
    }
    let db = context.db();
    let env = context.program_environment();
    let Type::NominalInstance(instance_ty) = receiver else {
        return None;
    };
    let base = instance_ty.class(db, env).class_literal(db).as_static()?;
    let base_name = bpy_types_class_name(db, base)?;
    if NO_SUBCLASS_MEMBERS.contains(&base_name) {
        return None;
    }
    let name = attribute.attr.id.as_str();
    let (ty, classes) = subclass_member(db, env, base, name)?;
    if let Some(builder) = context.report_lint(&BLENDER_ASSUMED_SUBCLASS_MEMBER, attribute) {
        let mut named = classes
            .iter()
            .take(NAMED_SUBCLASSES)
            .map(|class| format!("`{}`", class.name(db)))
            .collect::<Vec<_>>()
            .join(", ");
        if classes.len() > NAMED_SUBCLASSES {
            named.push_str(&format!(" and {} more", classes.len() - NAMED_SUBCLASSES));
        }
        let declarers = if classes.len() == 1 {
            format!("subclass {named} declares")
        } else {
            format!("subclasses {named} declare")
        };
        builder.into_diagnostic(format_args!(
            "`{base_name}` has no member `{name}`, only its {declarers} it: the struct is taken to \
             be one, so it is `{}`",
            ty.display(db, env),
        ));
    }
    Some(Place::bound(ty).into())
}

/// The union of the declarations of `name` by the tagged subclasses of `base` that declare it,
/// with those subclasses. `None` when none does.
fn subclass_member<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    base: StaticClassLiteral<'db>,
    name: &str,
) -> Option<(Type<'db>, Vec<StaticClassLiteral<'db>>)> {
    let mut classes = Vec::new();
    let mut types = Vec::new();
    for class in tagged_subclasses(db, base) {
        if let Some(ty) = instance(db, env, *class)
            .member(db, env, name)
            .place
            .ignore_possibly_undefined()
        {
            classes.push(*class);
            types.push(ty);
        }
    }
    if types.is_empty() {
        return None;
    }
    Some((UnionType::from_elements(db, env, types), classes))
}
