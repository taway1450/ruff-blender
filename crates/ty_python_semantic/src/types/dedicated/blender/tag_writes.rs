//! A write to a Type tag whose setter rebuilds the sibling pointers the tag implies
//! ([`ty_blender::rebuilds_on_tag_write`]): `fluid.fluid_type = 'DOMAIN'` makes Blender allocate
//! the modifier's `domain_settings`, so a later read of a member of `fluid` in the same block comes
//! from the tag view `_FluidModifier_DOMAIN`.
//!
//! ```python
//! fluid = obj.modifiers.new(name="Fluid", type='FLUID')
//! fluid.fluid_type = 'DOMAIN'
//! fluid.domain_settings.use_noise = True    # `FluidDomainSettings`, not `... | None`
//! ```
//!
//! By syntax alone, within one function or module-level script: the write is the last write of a
//! tag of that name before the read, a statement of a block that holds the read after it (so it
//! always ran before the read), and the name is not bound again between them. A write in a branch
//! the read is not in, or through another name, gives nothing.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_body, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_blender::rebuilds_on_tag_write;
use ty_python_core::global_scope;
use ty_python_core::scope::ScopeId;

use crate::Db;
use crate::place::PlaceAndQualifiers;
use crate::types::Type;
use crate::types::context::InferContext;

use super::context_members::enclosing_function;
use super::symbols::bpy_types_class_name;
use super::vector::module_class;

/// The member `attribute` of `receiver` read from the tag view an earlier tag write selected.
/// `None` leaves the read as it is.
pub(super) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    receiver: Type<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    if !attribute.ctx.is_load() {
        return None;
    }
    let Expr::Name(name) = &*attribute.value else {
        return None;
    };
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let Type::NominalInstance(instance) = receiver else {
        return None;
    };
    let class = instance.class(db, env).class_literal(db).as_static()?;
    let struct_name = bpy_types_class_name(db, class)?;
    let scope = enclosing_function(db, file, attribute).unwrap_or_else(|| global_scope(db, file));
    let writes = tag_writes(db, scope);
    let write = writes
        .writes
        .iter()
        .rev()
        .find(|write| write.name == name.id && write.statement.end() <= attribute.start())?;
    if attribute.start() > write.block_end
        || attribute.attr.id == write.attribute
        || !rebuilds_on_tag_write(struct_name, &write.attribute)
        || writes.bindings.iter().any(|(bound, range)| {
            *bound == name.id
                && range.start() >= write.statement.end()
                && range.end() <= attribute.start()
        })
    {
        return None;
    }
    let view = module_class(
        db,
        env,
        "bpy.types",
        &format!("_{struct_name}_{}", write.value),
    )?;
    let member = Type::instance(db, env, view.default_specialization(db)).member(
        db,
        env,
        attribute.attr.id.as_str(),
    );
    member.place.ignore_possibly_undefined()?;
    Some(member)
}

/// A statement `name.attribute = 'value'`.
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
struct TagWrite {
    statement: TextRange,
    /// The end of the block the statement is in.
    block_end: TextSize,
    name: Name,
    attribute: Name,
    value: Name,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, get_size2::GetSize)]
struct TagWrites {
    /// The writes of a string literal to an attribute of a name, in source order.
    writes: Box<[TagWrite]>,
    /// The names bound in the scope, with where.
    bindings: Box<[(Name, TextRange)]>,
}

/// The writes and bindings directly in `scope`, a function or a module; not those in a nested
/// function, class or lambda.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn tag_writes<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> TagWrites {
    let file = scope.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let body = match scope.node(db).as_function() {
        Some(function) => &function.node(&module).body[..],
        None if scope.file_scope_id(db).is_global() => &module.suite()[..],
        None => return TagWrites::default(),
    };
    let mut collector = Collector::default();
    collector.visit_body(body);
    TagWrites {
        writes: collector.writes.into_boxed_slice(),
        bindings: collector.bindings.into_boxed_slice(),
    }
}

#[derive(Default)]
struct Collector {
    writes: Vec<TagWrite>,
    bindings: Vec<(Name, TextRange)>,
}

impl<'a> Visitor<'a> for Collector {
    fn visit_body(&mut self, body: &'a [Stmt]) {
        if let Some(last) = body.last() {
            for statement in body {
                if let Stmt::Assign(ast::StmtAssign { targets, value, .. }) = statement
                    && let [Expr::Attribute(target)] = &targets[..]
                    && let Expr::Name(name) = &*target.value
                    && let Expr::StringLiteral(string) = &**value
                {
                    self.writes.push(TagWrite {
                        statement: statement.range(),
                        block_end: last.end(),
                        name: name.id.clone(),
                        attribute: target.attr.id.clone(),
                        value: Name::new(string.value.to_str()),
                    });
                }
            }
        }
        walk_body(self, body);
    }

    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if !matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
            walk_stmt(self, statement);
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Lambda(_) => {}
            Expr::Name(name) if !name.ctx.is_load() => {
                self.bindings.push((name.id.clone(), name.range()));
            }
            _ => walk_expr(self, expression),
        }
    }
}
