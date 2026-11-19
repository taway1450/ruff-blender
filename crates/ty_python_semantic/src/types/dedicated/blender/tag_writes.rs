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
//! By syntax alone, within one function or module-level script. Every write to an attribute of a
//! name is recorded: `fluid.fluid_type = kind`, `fluid.fluid_type += ...`, `del fluid.fluid_type`,
//! and `setattr(fluid, ...)`, which may write any attribute. A read narrows when the last such write
//! before it, of the tag or of an unknown attribute, is `name.tag = '<literal>'` in a block that
//! holds the read after it (so it always ran before the read), the name is not bound again between
//! them, and no loop around the read writes the tag or binds the name after it. Anything else, a
//! write in a branch the read is not in or a write through another name, gives nothing.

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
    let read = attribute.start();

    // The writes that may change a rebuilding tag of the name.
    let mut tag_writes = writes.writes.iter().filter(|write| {
        write.receiver == name.id
            && write
                .attribute
                .as_ref()
                .is_none_or(|tag| rebuilds_on_tag_write(struct_name, tag))
    });
    let write = tag_writes
        .clone()
        .rev()
        .find(|write| write.statement.end() <= read)?;
    let (Some(tag), Some(value)) = (&write.attribute, &write.value) else {
        return None;
    };
    // A read of the tag itself is typed by ty's own narrowing.
    if attribute.attr.id == *tag || read > write.block_end {
        return None;
    }
    // A loop around the read that writes the tag or binds the name after it.
    let in_loop_after = |range: TextRange| {
        range.start() > read
            && writes
                .loops
                .iter()
                .any(|body| body.contains_range(attribute.range()) && body.contains_range(range))
    };
    if tag_writes.any(|other| in_loop_after(other.statement))
        || writes.bindings.iter().any(|(bound, range)| {
            *bound == name.id
                && ((range.start() >= write.statement.end() && range.end() <= read)
                    || in_loop_after(*range))
        })
    {
        return None;
    }
    let view = module_class(db, env, "bpy.types", &format!("_{struct_name}_{value}"))?;
    let member = Type::instance(db, env, view.default_specialization(db)).member(
        db,
        env,
        attribute.attr.id.as_str(),
    );
    member.place.ignore_possibly_undefined()?;
    Some(member)
}

/// A write to an attribute of a name.
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
struct AttributeWrite {
    statement: TextRange,
    /// The end of the innermost block holding the statement.
    block_end: TextSize,
    /// The name whose attribute is written.
    receiver: Name,
    /// The attribute written, `None` for `setattr`, which may write any of them.
    attribute: Option<Name>,
    /// The string literal assigned, `None` for any other value.
    value: Option<Name>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, get_size2::GetSize)]
struct TagWrites {
    /// The writes to an attribute of a name, in source order.
    writes: Box<[AttributeWrite]>,
    /// The names bound in the scope, with where.
    bindings: Box<[(Name, TextRange)]>,
    /// The bodies of the `for` and `while` loops.
    loops: Box<[TextRange]>,
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
    let mut collector = AttributeWriteCollector::default();
    collector.visit_body(body);
    TagWrites {
        writes: collector.writes.into_boxed_slice(),
        bindings: collector.bindings.into_boxed_slice(),
        loops: collector.loops.into_boxed_slice(),
    }
}

#[derive(Default)]
struct AttributeWriteCollector {
    writes: Vec<AttributeWrite>,
    bindings: Vec<(Name, TextRange)>,
    loops: Vec<TextRange>,
    /// The ends of the blocks being visited, innermost last.
    block_ends: Vec<TextSize>,
}

impl AttributeWriteCollector {
    fn push(
        &mut self,
        range: TextRange,
        receiver: &Name,
        attribute: Option<Name>,
        value: Option<Name>,
    ) {
        self.writes.push(AttributeWrite {
            statement: range,
            block_end: self.block_ends.last().copied().unwrap_or(range.end()),
            receiver: receiver.clone(),
            attribute,
            value,
        });
    }
}

impl<'a> Visitor<'a> for AttributeWriteCollector {
    fn visit_body(&mut self, body: &'a [Stmt]) {
        if let Some(last) = body.last() {
            self.block_ends.push(last.end());
            walk_body(self, body);
            self.block_ends.pop();
        }
    }

    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            // `name.tag = 'VALUE'`: the one write whose value is known.
            Stmt::Assign(ast::StmtAssign { targets, value, .. })
                if let [Expr::Attribute(target)] = &targets[..]
                    && let Expr::Name(name) = &*target.value
                    && let Expr::StringLiteral(string) = &**value =>
            {
                self.push(
                    statement.range(),
                    &name.id,
                    Some(target.attr.id.clone()),
                    Some(Name::new(string.value.to_str())),
                );
            }
            Stmt::For(ast::StmtFor { body, .. }) | Stmt::While(ast::StmtWhile { body, .. }) => {
                if let (Some(first), Some(last)) = (body.first(), body.last()) {
                    self.loops.push(TextRange::new(first.start(), last.end()));
                }
                walk_stmt(self, statement);
            }
            _ => walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Lambda(_) => {}
            Expr::Name(name) if !name.ctx.is_load() => {
                self.bindings.push((name.id.clone(), name.range()));
            }
            // `name.attribute += ...`, `name.attribute = kind`, `del name.attribute`.
            Expr::Attribute(target)
                if !target.ctx.is_load()
                    && let Expr::Name(name) = &*target.value =>
            {
                self.push(target.range(), &name.id, Some(target.attr.id.clone()), None);
            }
            // `setattr(name, attribute, value)`, of an attribute the call may not show.
            Expr::Call(call)
                if let Expr::Name(function) = &*call.func
                    && function.id.as_str() == "setattr"
                    && let Some(Expr::Name(name)) = call.arguments.args.first() =>
            {
                self.push(call.range(), &name.id, None, None);
                walk_expr(self, expression);
            }
            _ => walk_expr(self, expression),
        }
    }
}
