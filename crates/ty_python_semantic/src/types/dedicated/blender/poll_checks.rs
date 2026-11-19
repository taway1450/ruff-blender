//! Narrowing by a `poll()`: a pointer the class's `poll()` checked is not `None` in the callbacks the
//! poll gates.
//!
//! Blender calls a Panel's `draw`, a Menu's `draw`, an Operator's `invoke` and `execute` and a
//! GizmoGroup's setup callbacks only right after `poll()` returned true, with the same context
//! ([`ty_blender::gated_by_poll`]). So what the poll checked holds there:
//!
//! ```python
//! class MY_PT_material(Panel):
//!     @classmethod
//!     def poll(cls, context):
//!         ob = context.object
//!         return ob is not None and ob.type == 'MESH'
//!
//!     def draw(self, context):
//!         context.object.data      # `Mesh`: reported under `blender-narrowed-by-poll`
//!         context.material.name    # still an error: the poll did not check it
//! ```
//!
//! [`poll_facts`] reads what a poll checks from its code, without inference: the conjuncts of its
//! `return` (`X`, `X is not None`, `isinstance(X, T)`, `bool(X)`, `type(X) is T` / `in {A, B}`,
//! `X.type == 'LIT'` / `in {...}`), the attribute reads always evaluated while it computes the
//! result (a poll that raises is a poll
//! that failed, so `ob.data.is_editmode` means `ob` and `ob.data` were set), early `return False`
//! guards, single-assignment aliases (also `getattr(context, "x", None)` and `(ob := context.x)`),
//! `bool(a and b)`, a `try` whose handlers never return true, `super().poll(context)` /
//! `Base.poll(context)`, known Blender functions ([`checked_by_call`]) and helper functions taking
//! the context, followed [`MAX_HELPER_DEPTH`] calls deep. A fact is about a
//! [`Path`] from the context, never about the class as a whole, so a pointer the poll did not check
//! keeps its stub type. `context.mode == 'EDIT_MESH'` gives what the mode implies
//! ([`ty_blender::mode_facts`]): an edit object of the mode's type, a paint mode's tool settings.
//! A fact about one side of a Pointer alias is also one about the other side
//! ([`ty_blender::implied_paths`]: a set `space_data.node_tree` means a set `edit_tree`), applied
//! without being reported. A Type tag test types the struct as the same test in the callback would
//! ([`narrow_to_tag_values`]), and one on `context.area` makes `context.space_data` the editor's
//! subclass, since a space is its area's first.
//!
//! The read is reported at the narrowed expression, unless the callback only tests it for truth or
//! `None` ([`FunctionPaths`]).
//!
//! Two more reaches use the same facts:
//!
//! - A sub-panel's own `poll` and the callbacks it gates also know what the polls up its
//!   `bl_parent_id` chain checked ([`parent_poll_facts`]), since Blender polls a sub-panel only
//!   while drawing its parent. The message names the parent.
//! - A read inside a `poll` knows what the poll's earlier guards checked ([`poll_flow`]). That is
//!   the poll's own control flow, so it is not reported.
//!
//! A Panel's `bl_context` gives facts the same way, where Blender enforces it before it polls the
//! panel ([`bl_context_facts`]): an object in the Properties tabs that need one, what the mode
//! gives in the 3D viewport's sidebar and toolbar. It holds in the panel's `poll` and the callbacks
//! the poll gates, and in those of its sub-panels, and is reported under
//! `blender-narrowed-by-bl-context`.

use std::cell::RefCell;

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashMap;
use ty_blender::{
    ModeFacts, bl_context_facts, checked_by_call, gated_by_poll, implied_paths, is_callback_name,
    mode_facts, python_base_names, same_pointers,
};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::{NodeWithScopeRef, ScopeId};
use ty_python_core::{ProgramFile, semantic_index};

use crate::place::PlaceAndQualifiers;
use crate::types::context::InferContext;
use crate::types::infer::nearest_enclosing_class;
use crate::types::{StaticClassLiteral, Type, UnionType};
use crate::{Db, ProgramEnvironment};

use super::classes::{class_string, mro_classes, own_binding};
use super::context_members::{enclosing_function, is_popover, space_subclass};
use super::follow::{Resolved, is_classmethod, resolve_expression};
use super::index::{addon_in_program, file_facts};
use super::lints::{BLENDER_NARROWED_BY_BL_CONTEXT, BLENDER_NARROWED_BY_POLL};
use super::member::without_none;
use super::overrides::callback_root;
use super::symbols::{bpy_types_class_name, is_bpy_types_class};
use super::type_tags::narrow_to_tag_values;

/// How many helper calls deep a poll is followed: `poll` → `poll_fluid_domain(context)` →
/// `_has_domain(context)` → `_modifier(context)`.
const MAX_HELPER_DEPTH: usize = 3;

/// How many aliases (`ob = context.object`, `data = ob.data`) are followed back at most.
const MAX_ALIAS_DEPTH: usize = 8;

/// How many panels up a `bl_parent_id` chain are followed to its top panel at most.
const MAX_PANEL_DEPTH: usize = 8;

/// An attribute chain from the context: `context.object.active_material` is
/// `["object", "active_material"]`, also read as `bpy.context.object.active_material` or through a
/// local `ob = context.object`.
type Path = Box<[Name]>;

/// What a `poll()` that returned true says about a path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
enum Fact {
    /// The pointer is not `None`.
    NotNone(Path),
    /// The pointer's Type tag `attribute` is one of `values` (`ob.type == 'MESH'`).
    Tag {
        path: Path,
        attribute: Name,
        values: Box<[Name]>,
    },
}

impl Fact {
    fn path(&self) -> &[Name] {
        match self {
            Fact::NotNone(path) | Fact::Tag { path, .. } => path,
        }
    }
}

/// Facts that hold together; a set, kept small.
#[derive(Clone, Debug, Default)]
struct Facts(Vec<Fact>);

impl Facts {
    fn add(&mut self, fact: Fact) {
        if !self.0.contains(&fact) {
            self.0.push(fact);
        }
    }

    fn extend(&mut self, other: Facts) {
        for fact in other.0 {
            self.add(fact);
        }
    }

    fn with(&self, other: Facts) -> Facts {
        let mut facts = self.clone();
        facts.extend(other);
        facts
    }

    /// The facts both hold.
    fn intersect(&self, other: &Facts) -> Facts {
        Facts(
            self.0
                .iter()
                .filter(|fact| other.0.contains(fact))
                .cloned()
                .collect(),
        )
    }

    fn not_none(&mut self, path: Option<Vec<Name>>) {
        if let Some(path) = path.filter(|path| !path.is_empty()) {
            self.add(Fact::NotNone(path.into_boxed_slice()));
        }
    }
}

/// The type of the attribute read `attribute`, where ty found `place`, when the read is in a
/// callback that a `poll()` gates and that poll checked its path, or in a `poll()` after its own
/// guards checked it. Reports the read when the gating poll's facts removed a `None` or replaced
/// `Space`, unless the callback only tests it or the poll's own guards already narrow it.
pub(super) fn on_load<'db>(
    context: &InferContext<'db, '_>,
    attribute: &ast::ExprAttribute,
    place: PlaceAndQualifiers<'db>,
) -> Option<PlaceAndQualifiers<'db>> {
    let db = context.db();
    let env = context.program_environment();
    let file = context.program_file();
    let function = enclosing_function(db, file, attribute)?;
    let gated = gated_facts(db, function).as_ref();
    let flow = poll_flow(db, function)
        .as_deref()
        .and_then(|flow| known_before(flow, attribute.range()));
    if gated.is_none() && flow.is_none() {
        return None;
    }
    let paths = function_paths(db, function);
    let path = paths.attribute_path(attribute)?;
    let applies = |fact: &Fact| {
        fact.path() == path.as_slice()
            || (path == ["space_data"]
                && matches!(fact, Fact::Tag { path, attribute, .. } if **path == ["area"] && attribute == "type"))
    };
    let gate_facts = gated.map(|gated| &gated.facts[..]).unwrap_or_default();
    let flow_facts = flow.unwrap_or_default();
    // What the Pointer aliases add is Blender's own bookkeeping, not an assumption about the
    // poll: like the poll's own control flow, it narrows without being reported.
    let aliased = alias_facts(
        gate_facts
            .iter()
            .map(|sourced| &sourced.fact)
            .chain(flow_facts),
    );
    let from_gate: Vec<&SourcedFact> = gate_facts
        .iter()
        .filter(|sourced| applies(&sourced.fact))
        .collect();
    let silent: Vec<&Fact> = flow_facts
        .iter()
        .chain(&aliased)
        .filter(|fact| applies(fact))
        .collect();
    let facts: Vec<&Fact> = from_gate
        .iter()
        .map(|sourced| &sourced.fact)
        .chain(silent.iter().copied())
        .collect();
    if facts.is_empty() {
        return None;
    }
    let original = place.place.ignore_possibly_undefined()?;
    let narrowed = narrow(db, env, original, &path, &facts);
    if narrowed == original {
        return None;
    }
    let removed_none = without_none(db, original) != original;
    // The poll's own control flow and the Pointer aliases are plain narrowing: only what the gating
    // poll adds to them is reported.
    let by_silent_alone = narrow(db, env, original, &path, &silent) == narrowed;
    if (removed_none || is_space(db, env, without_none(db, original)))
        && !by_silent_alone
        && let Some(gated) = gated
        && let Some(source) = from_gate.first()
        && !paths.skips(attribute.range(), &path)
    {
        let lint = match source.gate {
            Gate::BlContext(_) => &BLENDER_NARROWED_BY_BL_CONTEXT,
            Gate::Poll | Gate::ParentPoll => &BLENDER_NARROWED_BY_POLL,
        };
        if let Some(builder) = context.report_lint(lint, attribute) {
            let path = display_path(&path);
            let narrowed = narrowed.display(db, env);
            match &source.gate {
                Gate::BlContext(bl_context) => builder.into_diagnostic(format_args!(
                    "`{path}` is taken as `{narrowed}` because `{}` has `bl_context = \"{bl_context}\"`; \
                     Blender polls and draws the panel only where that context is current",
                    source.poll_class,
                )),
                gate => builder.into_diagnostic(format_args!(
                    "`{path}` is taken as `{narrowed}` because `{}.poll()` checked it; Blender calls \
                     `{}` only after {} returned true, with the same context",
                    source.poll_class,
                    gated.callback,
                    if matches!(gate, Gate::ParentPoll) {
                        "its parent panel's poll"
                    } else {
                        "its poll"
                    },
                )),
            };
        }
    }
    Some(place.map_type(|_| narrowed))
}

/// `ty`, the type of a read of `path`, narrowed by `facts` about it.
fn narrow<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    path: &[Name],
    facts: &[&Fact],
) -> Type<'db> {
    let mut ty = ty;
    for fact in facts {
        ty = without_none(db, ty);
        let Fact::Tag {
            path: fact_path,
            attribute,
            values,
        } = fact
        else {
            continue;
        };
        if **fact_path == *path {
            if let Some(tagged) = narrow_to_tag_values(db, env, ty, attribute, values) {
                ty = tagged;
            }
        } else if let Some(spaces) = values
            .iter()
            .map(|value| space_subclass(db, env, value))
            .collect::<Option<Vec<_>>>()
        {
            // `context.area.type == 'VIEW_3D'`: the space is the area's first.
            ty = UnionType::from_elements(db, env, spaces);
        }
    }
    ty
}

/// Whether `ty` is an instance of `bpy.types.Space` itself.
fn is_space<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> bool {
    let Type::NominalInstance(instance) = ty else {
        return false;
    };
    instance
        .class(db, env)
        .class_literal(db)
        .as_static()
        .is_some_and(|class| bpy_types_class_name(db, class) == Some("Space"))
}

fn display_path(path: &[Name]) -> String {
    let mut text = String::from("context");
    for name in path {
        text.push('.');
        text.push_str(name);
    }
    text
}

/// What a gated callback knows from the polls that ran before Blender called it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
struct GatedFacts {
    facts: Box<[SourcedFact]>,
    /// The callback's name, for the message.
    callback: Name,
}

/// A fact with the poll that checked it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
struct SourcedFact {
    fact: Fact,
    /// The class whose `poll` checked it, or whose `bl_context` gives it, for the message.
    poll_class: Name,
    gate: Gate,
}

/// What gives a [`SourcedFact`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
enum Gate {
    /// The class's own `poll`.
    Poll,
    /// The poll of a parent panel, up the `bl_parent_id` chain.
    ParentPoll,
    /// The `bl_context` of the panel, or of the top panel of its `bl_parent_id` chain.
    BlContext(Name),
}

/// The facts of the polls that gate the function scope `function`, when it is a method in the body
/// of a Blender class: the class's own poll for a callback it gates ([`gated_by_poll`]), and for a
/// Panel's gated callbacks and its own `poll` also the polls of the panels up its `bl_parent_id`
/// chain ([`parent_poll_facts`]). The callback is found by its name, as Blender looks it up.
///
/// Blender polls a sub-panel only while it draws the parent panel, and it draws the parent only
/// after the parent's poll returned true (`ed_panel_draw` in `area.cc`). A sub-panel that its
/// add-on also opens as a popover ([`is_popover`]) is drawn without its parent, so it gets no
/// parent facts. A Panel's `poll` and gated callbacks also get what its `bl_context` gives
/// ([`panel_context_facts`]).
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn gated_facts<'db>(db: &'db dyn Db, function: ScopeId<'db>) -> Option<GatedFacts> {
    let file = function.program_file(db);
    let index = semantic_index(db, file);
    let in_class_body = index
        .parent_scope_id(function.file_scope_id(db))
        .is_some_and(|parent| index.scope(parent).node().as_class().is_some());
    let node = function.node(db).as_function().filter(|_| in_class_body)?;
    let module = parsed_module(db, file.python_file(db)).load(db);
    let name = node.node(&module).name.id.clone();
    let class = nearest_enclosing_class(db, index, function)?;
    let root = callback_root(db, class)?;
    let gated = gated_by_poll(root, &name);
    let is_panel = root == "Panel";
    if !gated && !(is_panel && name == "poll") {
        return None;
    }
    let mut facts: Vec<SourcedFact> = Vec::new();
    if gated && let Some((own, poll_class)) = poll_facts(db, class).as_ref() {
        facts.extend(own.iter().map(|fact| SourcedFact {
            fact: fact.clone(),
            poll_class: poll_class.clone(),
            gate: Gate::Poll,
        }));
    }
    if is_panel && !is_popover(db, file, class) {
        for sourced in parent_poll_facts(db, class).as_deref().unwrap_or_default() {
            if !facts.iter().any(|known| known.fact == sourced.fact) {
                facts.push(sourced.clone());
            }
        }
        for sourced in panel_context_facts(db, class)
            .as_deref()
            .unwrap_or_default()
        {
            if !facts.iter().any(|known| known.fact == sourced.fact) {
                facts.push(sourced.clone());
            }
        }
    }
    if facts.is_empty() {
        return None;
    }
    Some(GatedFacts {
        facts: facts.into_boxed_slice(),
        callback: name,
    })
}

/// What the polls of the panels up the `bl_parent_id` chain of the panel `class` check, each fact
/// with the panel whose poll checked it. The parent is the panel whose idname (its `bl_idname`, or
/// else its class name) is the `bl_parent_id`, looked up among the classes of the same file, then
/// of the same add-on. A parent that is one of Blender's own panels is not found, and gives
/// nothing.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn parent_poll_facts<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<Box<[SourcedFact]>> {
    let parent = parent_panel(db, class)?;
    let mut facts: Vec<SourcedFact> = Vec::new();
    if let Some((own, poll_class)) = poll_facts(db, parent).as_ref() {
        facts.extend(own.iter().map(|fact| SourcedFact {
            fact: fact.clone(),
            poll_class: poll_class.clone(),
            gate: Gate::ParentPoll,
        }));
    }
    for sourced in parent_poll_facts(db, parent).as_deref().unwrap_or_default() {
        if !facts.iter().any(|known| known.fact == sourced.fact) {
            facts.push(sourced.clone());
        }
    }
    Some(facts.into_boxed_slice())
}

/// The parent panel of the panel `class`: the panel whose idname (its `bl_idname`, or else its
/// class name) is the `bl_parent_id`, looked up among the classes of the same file, then of the
/// same add-on. `None` without a `bl_parent_id`, or for one of Blender's own panels.
fn parent_panel<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<StaticClassLiteral<'db>> {
    let parent_id = class_string(db, class, "bl_parent_id")?;
    let file = class.body_scope(db).program_file(db);
    let find = |file: ProgramFile<'db>| {
        file_facts(db, file)
            .classes
            .iter()
            .map(|candidate| candidate.class)
            .find(|candidate| {
                *candidate != class
                    && class_string(db, *candidate, "bl_idname")
                        .unwrap_or_else(|| candidate.name(db).to_string())
                        == parent_id
            })
    };
    find(file)
        .or_else(|| addon_in_program(db, file).and_then(|addon| addon.files(db).find_map(find)))
}

/// What the `bl_context` of the panel `class` gives where Blender enforces it
/// ([`bl_context_facts`]), each fact with the panel that declares it. Blender checks the context
/// of a top-level panel only, before its poll, and draws a sub-panel only inside its parent, so the
/// context is that of the top panel of the `bl_parent_id` chain. Nothing when a panel of the chain
/// is not found or is opened as a popover ([`is_popover`]), where no context is checked.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn panel_context_facts<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Option<Box<[SourcedFact]>> {
    let mut top = class;
    for _ in 0..MAX_PANEL_DEPTH {
        if is_popover(db, top.body_scope(db).program_file(db), top) {
            return None;
        }
        if class_string(db, top, "bl_parent_id").is_none() {
            let space_type = class_string(db, top, "bl_space_type")?;
            // Blender's default region type is the main region.
            let region_type =
                class_string(db, top, "bl_region_type").unwrap_or_else(|| "WINDOW".to_string());
            let bl_context = class_string(db, top, "bl_context")?;
            let known = bl_context_facts(&space_type, &region_type, &bl_context)?;
            let declaring = class_attribute_owner(db, top, "bl_context").unwrap_or(top);
            return Some(
                known_facts(known)
                    .0
                    .into_iter()
                    .map(|fact| SourcedFact {
                        fact,
                        poll_class: declaring.name(db).clone(),
                        gate: Gate::BlContext(Name::new(&bl_context)),
                    })
                    .collect(),
            );
        }
        top = parent_panel(db, top)?;
    }
    None
}

/// The class of the method resolution order of `class` whose body binds `name`, before the
/// `bpy.types` classes.
fn class_attribute_owner<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<StaticClassLiteral<'db>> {
    mro_classes(db, class)
        .take_while(|base| !is_bpy_types_class(db, *base))
        .find(|base| own_binding(db, *base, name).is_some())
}

/// What the `poll` Blender calls for `class` checks, with the name of the class that defines it:
/// the first `poll` in its method resolution order that is not a `bpy.types` class's
/// ([`method_after`]). `None` for a class without one.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn poll_facts<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> Option<(Box<[Fact]>, Name)> {
    let (definition, defining) = method_on(db, class, "poll")?;
    let facts = function_facts(db, definition, Owner::Class { class, defining }, 1, None, 0)?;
    Some((facts.0.into_boxed_slice(), defining.name(db).clone()))
}

/// The method `name` of `class`: its definition and the class whose body defines it, the first in
/// the method resolution order ([`method_after`]).
fn method_on<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<(Definition<'db>, StaticClassLiteral<'db>)> {
    method_after(db, class, None, name)
}

/// The method `name` of `class` found after `after` in its method resolution order
/// (`super().name` in a method of `after`), or from the start without one.
///
/// A `bpy.types` class ends the search, unless `name` is a Blender callback its Python base does
/// not define: Blender's `Operator` or `Menu` has no `poll` attribute (the callback is looked up
/// on the registered class), so `class X(Operator, Mixin)` uses `Mixin.poll`, while `Node.poll`
/// is a Python base name and wins over a mixin listed after `Node`.
fn method_after<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    after: Option<StaticClassLiteral<'db>>,
    name: &str,
) -> Option<(Definition<'db>, StaticClassLiteral<'db>)> {
    let mut mro = mro_classes(db, class).peekable();
    if let Some(after) = after {
        for base in mro.by_ref() {
            if base == after {
                break;
            }
        }
    }
    for base in mro {
        if let Some(blender_name) = bpy_types_class_name(db, base) {
            if is_callback_name(name) && !python_base_names(blender_name).contains(&name) {
                continue;
            }
            return None;
        }
        if let Some((definition, _)) = own_binding(db, base, name) {
            return matches!(definition.kind(db), DefinitionKind::Function(_))
                .then_some((definition, base));
        }
    }
    None
}

/// The class a method is read for: `class` is the class Blender calls it on, `defining` the class
/// whose body defines it (`super()` starts after it). A plain function has none.
#[derive(Copy, Clone)]
enum Owner<'db> {
    Class {
        class: StaticClassLiteral<'db>,
        defining: StaticClassLiteral<'db>,
    },
    Function,
}

/// What a `poll()` knows before each of its statements and test operands, from its own earlier
/// guards: `if not helper(context): return False` makes what `helper` checks known after it, and
/// `helper(context) and context.object.mode` reads `context.object` knowing it. Each entry is the
/// range of a statement or an expression, with the facts known when it is evaluated.
type PollFlow = Box<[(TextRange, Box<[Fact]>)]>;

/// The facts known in the function scope `function` when it is the `poll` of a Blender class,
/// by its own control flow ([`PollFlow`]). A read uses the entry of the smallest range around it
/// ([`known_before`]).
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn poll_flow<'db>(db: &'db dyn Db, function: ScopeId<'db>) -> Option<PollFlow> {
    let file = function.program_file(db);
    let index = semantic_index(db, file);
    let in_class_body = index
        .parent_scope_id(function.file_scope_id(db))
        .is_some_and(|parent| index.scope(parent).node().as_class().is_some());
    let node = function.node(db).as_function().filter(|_| in_class_body)?;
    let module = parsed_module(db, file.python_file(db)).load(db);
    let node = node.node(&module);
    if node.name.id != "poll" {
        return None;
    }
    let class = nearest_enclosing_class(db, index, function)?;
    callback_root(db, class)?;
    let parameters = &node.parameters;
    let positional: Vec<&Name> = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .map(|parameter| &parameter.parameter.name.id)
        .collect();
    let context = *positional.get(1)?;
    let aliases = aliases(Some(parameters), &node.body, context);
    let reader = PollReader {
        db,
        scope: function,
        context,
        first: positional.first().copied(),
        aliases: &aliases,
        owner: Owner::Class {
            class,
            defining: class,
        },
        depth: 0,
        recording: Some(RefCell::new(Vec::new())),
    };
    reader.block(&node.body, Facts::default());
    let flow: PollFlow = reader
        .recording
        .map(RefCell::into_inner)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, facts)| !facts.0.is_empty())
        .map(|(range, facts)| (range, facts.0.into_boxed_slice()))
        .collect();
    (!flow.is_empty()).then_some(flow)
}

/// The facts known when the expression at `range` in a poll is evaluated: those of the smallest
/// recorded statement or operand around it.
fn known_before(flow: &[(TextRange, Box<[Fact]>)], range: TextRange) -> Option<&[Fact]> {
    flow.iter()
        .filter(|(recorded, _)| recorded.contains_range(range))
        .min_by_key(|(recorded, _)| recorded.len())
        .map(|(_, facts)| &facts[..])
}

/// The facts that hold whenever the function `definition` returns a true value, its context being
/// the parameter at `position` (or the one named `keyword`). `None` when it never returns one.
fn function_facts<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    owner: Owner<'db>,
    position: usize,
    keyword: Option<&Name>,
    depth: usize,
) -> Option<Facts> {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let file = definition.program_file(db);
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let function = function.node(&module);
    let parameters = &function.parameters;
    let positional: Vec<&Name> = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .map(|parameter| &parameter.parameter.name.id)
        .collect();
    let context = match keyword {
        Some(keyword) => parameters
            .iter_non_variadic_params()
            .map(|parameter| &parameter.parameter.name.id)
            .find(|name| *name == keyword)?,
        None => *positional.get(position)?,
    };
    let scope = semantic_index(db, file)
        .node_scope(NodeWithScopeRef::Function(function))
        .to_scope_id(db, file);
    let aliases = aliases(Some(&function.parameters), &function.body, context);
    let reader = PollReader {
        db,
        scope,
        context,
        first: positional.first().copied(),
        aliases: &aliases,
        owner,
        depth,
        recording: None,
    };
    let flow = reader.block(&function.body, Facts::default());
    let mut returns = flow.returns.into_iter();
    let first = returns.next()?;
    Some(returns.fold(first, |facts, other| facts.intersect(&other)))
}

/// Where the statements of a block lead.
struct Flow {
    /// The facts at the end of the block, when it can fall through.
    fallthrough: Option<Facts>,
    /// The facts at each `return` that may give a true value.
    returns: Vec<Facts>,
}

struct PollReader<'db, 'a> {
    db: &'db dyn Db,
    /// The function's body scope, where its names are resolved.
    scope: ScopeId<'db>,
    context: &'a Name,
    /// The first parameter, `cls` or `self` in a method.
    first: Option<&'a Name>,
    aliases: &'a FxHashMap<Name, Alias>,
    owner: Owner<'db>,
    depth: usize,
    /// When set, the facts known before each statement and test operand of the body, for the
    /// reads of a poll itself ([`poll_flow`]).
    recording: Option<RefCell<Vec<(TextRange, Facts)>>>,
}

impl<'db> PollReader<'db, '_> {
    /// Records the facts known before `expression` and, through `and`, `or` and `not`, before
    /// each of its operands.
    fn record(&self, expression: &Expr, facts: &Facts) {
        let Some(recording) = &self.recording else {
            return;
        };
        recording
            .borrow_mut()
            .push((expression.range(), facts.clone()));
        match expression {
            Expr::BoolOp(ast::ExprBoolOp { op, values, .. }) => {
                let mut known = facts.clone();
                for value in values {
                    self.record(value, &known);
                    match op {
                        ast::BoolOp::And => known.extend(self.truthy(value)),
                        ast::BoolOp::Or => known.extend(self.falsy(value)),
                    }
                }
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => self.record(operand, facts),
            _ => {}
        }
    }

    fn record_statement(&self, statement: &Stmt, facts: &Facts) {
        if let Some(recording) = &self.recording {
            recording
                .borrow_mut()
                .push((statement.range(), facts.clone()));
        }
    }

    fn path(&self, expression: &Expr) -> Option<Vec<Name>> {
        path_of(expression, self.context, self.aliases)
    }

    fn block(&self, body: &[Stmt], facts: Facts) -> Flow {
        let mut facts = facts;
        let mut returns = Vec::new();
        for statement in body {
            self.record_statement(statement, &facts);
            match statement {
                Stmt::Return(ast::StmtReturn { value, .. }) => {
                    if let Some(value) = value.as_deref() {
                        self.record(value, &facts);
                    }
                    if let Some(value) = value.as_deref()
                        && !is_false(value)
                    {
                        returns.push(facts.with(self.truthy(value)));
                    }
                    return Flow {
                        fallthrough: None,
                        returns,
                    };
                }
                Stmt::Raise(_) => {
                    return Flow {
                        fallthrough: None,
                        returns,
                    };
                }
                Stmt::If(if_statement) => {
                    self.record(&if_statement.test, &facts);
                    let mut ends: Vec<Facts> = Vec::new();
                    let mut negated = facts.clone();
                    let body = self.block(
                        &if_statement.body,
                        negated.with(self.truthy(&if_statement.test)),
                    );
                    returns.extend(body.returns);
                    ends.extend(body.fallthrough);
                    negated.extend(self.falsy(&if_statement.test));
                    let mut has_else = false;
                    for clause in &if_statement.elif_else_clauses {
                        let arm = match &clause.test {
                            Some(test) => {
                                self.record(test, &negated);
                                let arm = self.block(&clause.body, negated.with(self.truthy(test)));
                                negated.extend(self.falsy(test));
                                arm
                            }
                            None => {
                                has_else = true;
                                self.block(&clause.body, negated.clone())
                            }
                        };
                        returns.extend(arm.returns);
                        ends.extend(arm.fallthrough);
                    }
                    if !has_else {
                        ends.push(negated);
                    }
                    let mut ends = ends.into_iter();
                    let Some(first) = ends.next() else {
                        return Flow {
                            fallthrough: None,
                            returns,
                        };
                    };
                    facts = ends.fold(first, |facts, other| facts.intersect(&other));
                }
                Stmt::Expr(ast::StmtExpr { value, .. })
                | Stmt::Assign(ast::StmtAssign { value, .. })
                | Stmt::AugAssign(ast::StmtAugAssign { value, .. })
                | Stmt::AnnAssign(ast::StmtAnnAssign {
                    value: Some(value), ..
                }) => {
                    self.record(value, &facts);
                    facts.extend(self.reads(value));
                }
                // `try: return context.object.data.is_editmode` with `except AttributeError:
                // return False`: a true value can only come from the body, after its reads.
                Stmt::Try(try_statement) if follows_try(try_statement) => {
                    let body = self.block(&try_statement.body, facts.clone());
                    returns.extend(body.returns);
                    // After an exception, only what was known before the `try` holds.
                    let mut ends: Vec<Facts> = Vec::new();
                    if body.fallthrough.is_some() {
                        ends.push(facts.clone());
                    }
                    for handler in &try_statement.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        ends.extend(self.block(&handler.body, facts.clone()).fallthrough);
                    }
                    let mut ends = ends.into_iter();
                    let Some(first) = ends.next() else {
                        return Flow {
                            fallthrough: None,
                            returns,
                        };
                    };
                    facts = ends.fold(first, |facts, other| facts.intersect(&other));
                }
                Stmt::FunctionDef(_)
                | Stmt::ClassDef(_)
                | Stmt::Pass(_)
                | Stmt::Import(_)
                | Stmt::ImportFrom(_)
                | Stmt::AnnAssign(_)
                | Stmt::Global(_)
                | Stmt::Nonlocal(_)
                | Stmt::Delete(_)
                | Stmt::TypeAlias(_) => {}
                // A loop, `with`, `try` or `match` is not followed: a return inside it keeps only
                // what was known before it.
                _ => {
                    if may_return_true(std::slice::from_ref(statement)) {
                        returns.push(facts.clone());
                    }
                }
            }
        }
        Flow {
            fallthrough: Some(facts),
            returns,
        }
    }

    /// The facts when `test` gives a true value.
    fn truthy(&self, test: &Expr) -> Facts {
        let mut facts = Facts::default();
        match test {
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::And,
                values,
                ..
            }) => {
                for value in values {
                    facts.extend(self.truthy(value));
                }
            }
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::Or,
                values,
                ..
            }) => {
                let mut arms = values.iter().map(|value| self.truthy(value));
                if let Some(first) = arms.next() {
                    facts.extend(arms.fold(first, |facts, other| facts.intersect(&other)));
                }
                if let Some(first) = values.first() {
                    facts.extend(self.reads(first));
                }
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => facts.extend(self.falsy(operand)),
            Expr::Compare(compare) => {
                facts.extend(self.reads(test));
                if let Some((operand, op, other)) = single_comparison(compare) {
                    match op {
                        ast::CmpOp::IsNot | ast::CmpOp::NotEq if other.is_none_literal_expr() => {
                            facts.not_none(self.path(operand));
                        }
                        ast::CmpOp::Eq | ast::CmpOp::In => facts.extend(self.tag(operand, other)),
                        _ => {}
                    }
                }
                if let Some(checked) = type_test_argument(compare, true) {
                    facts.not_none(self.path(checked));
                }
            }
            Expr::Call(call) if let Some(value) = bool_argument(call) => {
                facts.extend(self.truthy(value));
            }
            Expr::Call(call) => {
                facts.extend(self.reads(test));
                if self.path(test).is_some() {
                    // `getattr(context, "object", None)`
                    facts.not_none(self.path(test));
                } else if let Some(checked) = checked_argument(call) {
                    facts.not_none(self.path(checked));
                } else if let Some(checked) = self.known_call(call) {
                    facts.not_none(Some(checked));
                } else if let Some(helper) = self.call_facts(call) {
                    facts.extend(helper);
                }
            }
            Expr::Name(_) | Expr::Attribute(_) | Expr::Named(_) => {
                facts.extend(self.reads(test));
                facts.not_none(self.path(test));
            }
            _ => facts.extend(self.reads(test)),
        }
        facts
    }

    /// The facts when `test` gives a false value.
    fn falsy(&self, test: &Expr) -> Facts {
        let mut facts = Facts::default();
        match test {
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::Or,
                values,
                ..
            }) => {
                for value in values {
                    facts.extend(self.falsy(value));
                }
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => facts.extend(self.truthy(operand)),
            Expr::Call(call) if let Some(value) = bool_argument(call) => {
                facts.extend(self.falsy(value));
            }
            Expr::Compare(compare) => {
                facts.extend(self.reads(test));
                if let Some((operand, op, other)) = single_comparison(compare) {
                    match op {
                        ast::CmpOp::Is | ast::CmpOp::Eq if other.is_none_literal_expr() => {
                            facts.not_none(self.path(operand));
                        }
                        ast::CmpOp::NotEq | ast::CmpOp::NotIn => {
                            facts.extend(self.tag(operand, other));
                        }
                        _ => {}
                    }
                }
                if let Some(checked) = type_test_argument(compare, false) {
                    facts.not_none(self.path(checked));
                }
            }
            _ => facts.extend(self.reads(test)),
        }
        facts
    }

    /// The facts of `<struct>.<attribute> == <literal>` (or `in` a literal collection) when it
    /// holds: the struct is not `None`, and its tag `attribute` is one of the literals.
    fn tag(&self, operand: &Expr, other: &Expr) -> Facts {
        let mut facts = Facts::default();
        let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = operand else {
            return facts;
        };
        let Some(values) = string_literals(other) else {
            return facts;
        };
        let Some(path) = self.path(value) else {
            return facts;
        };
        if path.is_empty() {
            if attr.id == "mode" {
                facts.extend(mode_facts_of(&values));
            }
            return facts;
        }
        facts.add(Fact::Tag {
            path: path.into_boxed_slice(),
            attribute: attr.id.clone(),
            values: values.into_boxed_slice(),
        });
        facts
    }

    /// The attribute reads always evaluated while `expression` is: each makes its receiver not
    /// `None`, since a poll that raises fails. Only the first operand of `and` / `or` and the test
    /// of a conditional expression are always evaluated; lambdas and comprehensions are skipped.
    fn reads(&self, expression: &Expr) -> Facts {
        let mut facts = Facts::default();
        self.collect_reads(expression, &mut facts);
        facts
    }

    fn collect_reads(&self, expression: &Expr, facts: &mut Facts) {
        match expression {
            Expr::Attribute(ast::ExprAttribute { value, .. }) => {
                facts.not_none(self.path(value));
                self.collect_reads(value, facts);
            }
            Expr::Call(call) => {
                self.collect_reads(&call.func, facts);
                for argument in &call.arguments.args {
                    self.collect_reads(argument, facts);
                }
                for keyword in &call.arguments.keywords {
                    self.collect_reads(&keyword.value, facts);
                }
            }
            Expr::Compare(compare) => {
                for operand in compare.operands.iter().take(2) {
                    self.collect_reads(operand, facts);
                }
            }
            Expr::BoolOp(ast::ExprBoolOp { values, .. }) => {
                if let Some(first) = values.first() {
                    self.collect_reads(first, facts);
                }
            }
            Expr::BinOp(ast::ExprBinOp { left, right, .. }) => {
                self.collect_reads(left, facts);
                self.collect_reads(right, facts);
            }
            Expr::UnaryOp(ast::ExprUnaryOp { operand, .. }) => self.collect_reads(operand, facts),
            Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
                self.collect_reads(value, facts);
                self.collect_reads(slice, facts);
            }
            Expr::If(ast::ExprIf { test, .. }) => self.collect_reads(test, facts),
            Expr::Tuple(ast::ExprTuple { elts, .. })
            | Expr::List(ast::ExprList { elts, .. })
            | Expr::Set(ast::ExprSet { elts, .. }) => {
                for element in elts {
                    self.collect_reads(element, facts);
                }
            }
            Expr::Starred(ast::ExprStarred { value, .. })
            | Expr::Named(ast::ExprNamed { value, .. }) => self.collect_reads(value, facts),
            _ => {}
        }
    }

    /// The path a call of a known Blender function on the context checks when it returns true
    /// ([`checked_by_call`]): `XrSessionState.is_running(context)`.
    fn known_call(&self, call: &ast::ExprCall) -> Option<Vec<Name>> {
        let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = &*call.func else {
            return None;
        };
        let [argument] = &*call.arguments.args else {
            return None;
        };
        if !call.arguments.keywords.is_empty()
            || !self.path(argument).is_some_and(|path| path.is_empty())
        {
            return None;
        }
        let Some(Resolved::Value(Type::ClassLiteral(class))) =
            resolve_expression(self.db, self.scope, value)
        else {
            return None;
        };
        let class = bpy_types_class_name(self.db, class.as_static()?)?;
        let path = checked_by_call(class, attr.id.as_str())?;
        Some(path.iter().map(|name| Name::new(name)).collect())
    }

    /// The facts of a helper `call` that is passed the context: a function (`helper(context)`,
    /// `module.helper(context)`), a method of the class (`cls._poll(context)`), of a named class
    /// (`Base.poll(context)`) or of the next class (`super().poll(context)`).
    fn call_facts(&self, call: &ast::ExprCall) -> Option<Facts> {
        if self.depth >= MAX_HELPER_DEPTH {
            return None;
        }
        let is_context =
            |expression: &Expr| self.path(expression).is_some_and(|path| path.is_empty());
        let position = call.arguments.args.iter().position(is_context);
        let keyword = call
            .arguments
            .keywords
            .iter()
            .find(|keyword| is_context(&keyword.value))
            .and_then(|keyword| keyword.arg.as_ref())
            .map(|keyword| keyword.id.clone());
        if position.is_none() && keyword.is_none() {
            return None;
        }
        let (definition, owner, bound) = self.callee(&call.func)?;
        let (position, keyword) = match position {
            Some(position) => (position + usize::from(bound), None),
            None => (0, keyword.as_ref()),
        };
        function_facts(
            self.db,
            definition,
            owner,
            position,
            keyword,
            self.depth + 1,
        )
    }

    /// The function `callee` calls, the class it is read for, and whether the call binds its first
    /// parameter (a method read from a class or instance, a classmethod).
    fn callee(&self, callee: &Expr) -> Option<(Definition<'db>, Owner<'db>, bool)> {
        let db = self.db;
        if let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = callee {
            let name = attr.id.as_str();
            // `super().poll(context)`
            if let Expr::Call(ast::ExprCall {
                func, arguments, ..
            }) = &**value
                && func.as_name_expr().is_some_and(|func| func.id == "super")
                && arguments.is_empty()
            {
                let Owner::Class { class, defining } = self.owner else {
                    return None;
                };
                let (definition, defining) = method_after(db, class, Some(defining), name)?;
                return Some((definition, Owner::Class { class, defining }, true));
            }
            // `cls._poll(context)`, `self._poll(context)`
            if let Expr::Name(receiver) = &**value
                && self.first == Some(&receiver.id)
                && let Owner::Class { class, .. } = self.owner
            {
                let (definition, defining) = method_on(db, class, name)?;
                let bound = !is_staticmethod(db, definition);
                return Some((definition, Owner::Class { class, defining }, bound));
            }
            // `Base.poll(context)`
            if let Some(Resolved::Value(Type::ClassLiteral(class))) =
                resolve_expression(db, self.scope, value)
                && let Some(class) = class.as_static()
            {
                let (definition, defining) = method_on(db, class, name)?;
                let bound = is_classmethod_definition(db, definition);
                return Some((definition, Owner::Class { class, defining }, bound));
            }
        }
        match resolve_expression(db, self.scope, callee)? {
            Resolved::Function(definition) => Some((definition, Owner::Function, false)),
            _ => None,
        }
    }
}

fn is_staticmethod(db: &dyn Db, definition: Definition<'_>) -> bool {
    has_decorator(db, definition, "staticmethod")
}

fn is_classmethod_definition(db: &dyn Db, definition: Definition<'_>) -> bool {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return false;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    is_classmethod(function.node(&module))
}

fn has_decorator(db: &dyn Db, definition: Definition<'_>, name: &str) -> bool {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return false;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    function
        .node(&module)
        .decorator_list
        .iter()
        .any(|decorator| {
            decorator
                .expression
                .as_name_expr()
                .is_some_and(|decorator| decorator.id == name)
        })
}

/// Whether the poll reader follows the `try` statement: one with handlers that never return a true
/// value, and no `else` or `finally`.
fn follows_try(statement: &ast::StmtTry) -> bool {
    statement.orelse.is_empty()
        && statement.finalbody.is_empty()
        && !statement.handlers.is_empty()
        && statement.handlers.iter().all(|handler| {
            let ast::ExceptHandler::ExceptHandler(handler) = handler;
            !may_return_true(&handler.body)
        })
}

/// Whether `expression` is a literal false value: `False`, `None` or `0`.
fn is_false(expression: &Expr) -> bool {
    match expression {
        Expr::BooleanLiteral(literal) => !literal.value,
        Expr::NoneLiteral(_) => true,
        Expr::NumberLiteral(ast::ExprNumberLiteral {
            value: ast::Number::Int(value),
            ..
        }) => *value == 0,
        _ => false,
    }
}

/// Whether `body` holds a `return` of anything but a literal false value, outside nested
/// functions and classes.
fn may_return_true(body: &[Stmt]) -> bool {
    body.iter().any(|statement| match statement {
        Stmt::Return(ast::StmtReturn { value, .. }) => {
            value.as_deref().is_some_and(|value| !is_false(value))
        }
        Stmt::If(statement) => {
            may_return_true(&statement.body)
                || statement
                    .elif_else_clauses
                    .iter()
                    .any(|clause| may_return_true(&clause.body))
        }
        Stmt::For(statement) => {
            may_return_true(&statement.body) || may_return_true(&statement.orelse)
        }
        Stmt::While(statement) => {
            may_return_true(&statement.body) || may_return_true(&statement.orelse)
        }
        Stmt::With(statement) => may_return_true(&statement.body),
        Stmt::Try(statement) => {
            may_return_true(&statement.body)
                || may_return_true(&statement.orelse)
                || may_return_true(&statement.finalbody)
                || statement.handlers.iter().any(|handler| {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    may_return_true(&handler.body)
                })
        }
        Stmt::Match(statement) => statement
            .cases
            .iter()
            .any(|case| may_return_true(&case.body)),
        _ => false,
    })
}

/// What `context.mode` being one of `modes` says about the context ([`mode_facts`]): the facts
/// every one of the modes gives. Nothing when one of them gives none (`OBJECT`).
fn mode_facts_of(modes: &[Name]) -> Facts {
    let mut each = modes
        .iter()
        .map(|mode| mode_facts(mode.as_str()).map(known_facts));
    let Some(Some(first)) = each.next() else {
        return Facts::default();
    };
    each.try_fold(first, |facts, other| Some(facts.intersect(&other?)))
        .unwrap_or_default()
}

/// The facts of a mode or a `bl_context` ([`ModeFacts`]): its paths are set, and its one-member
/// paths have its object type.
fn known_facts(known: ModeFacts) -> Facts {
    let mut facts = Facts::default();
    for path in known.set {
        let path: Path = path.iter().map(|name| Name::new_static(name)).collect();
        if let Some(object_type) = known.object_type
            && path.len() == 1
        {
            facts.add(Fact::Tag {
                path: path.clone(),
                attribute: Name::new_static("type"),
                values: Box::new([Name::new_static(object_type)]),
            });
        }
        facts.add(Fact::NotNone(path));
    }
    facts
}

/// The facts the Pointer aliases add to `facts` ([`implied_paths`], [`same_pointers`]): a set
/// `space_data.node_tree` makes `space_data.edit_tree` set, a set or tagged `context.active_node`
/// makes `space_data.edit_tree.nodes.active` so, and the reverse. Only those not in `facts`.
fn alias_facts<'f>(facts: impl IntoIterator<Item = &'f Fact>) -> Vec<Fact> {
    let mut known: Vec<Fact> = facts.into_iter().cloned().collect();
    let given = known.len();
    let mut next = 0;
    while next < known.len() {
        let fact = known[next].clone();
        next += 1;
        let path = fact.path();
        let mut added: Vec<Fact> = implied_paths(path)
            .into_iter()
            .map(|implied| {
                Fact::NotNone(implied.iter().map(|name| Name::new_static(name)).collect())
            })
            .collect();
        if let Fact::Tag {
            attribute, values, ..
        } = &fact
        {
            added.extend(same_pointers(path).into_iter().map(|same| Fact::Tag {
                path: same.iter().map(|name| Name::new_static(name)).collect(),
                attribute: attribute.clone(),
                values: values.clone(),
            }));
        }
        for fact in added {
            if !known.contains(&fact) {
                known.push(fact);
            }
        }
    }
    known.split_off(given)
}

/// The operand, operator and other side of a comparison with one operator, with the `None` literal
/// put on the right: `None is X` is read as `X is None`.
pub(super) fn single_comparison(compare: &ast::ExprCompare) -> Option<(&Expr, ast::CmpOp, &Expr)> {
    let (left, op, right) = compare.as_single()?;
    if left.is_none_literal_expr()
        && matches!(
            op,
            ast::CmpOp::Is | ast::CmpOp::IsNot | ast::CmpOp::Eq | ast::CmpOp::NotEq
        )
    {
        return Some((right, *op, left));
    }
    Some((left, *op, right))
}

/// The argument of `isinstance(X, T)` or `bool(X)`, which is true only when `X` is not `None`.
fn checked_argument(call: &ast::ExprCall) -> Option<&Expr> {
    let Expr::Name(function) = &*call.func else {
        return None;
    };
    match (function.id.as_str(), &*call.arguments.args) {
        ("isinstance", [checked, _]) | ("bool", [checked])
            if call.arguments.keywords.is_empty() =>
        {
            Some(checked)
        }
        _ => None,
    }
}

/// The `X` of a test of its exact type that holds only when `X` is not `None`: when `holds`,
/// `type(X) is T`, `type(X) == T` or `type(X) in {A, B}` (a set, tuple or list of classes); when
/// not, the negated forms (`is not`, `!=`, `not in`), which are false only then. The classes are
/// names or attributes, never `NoneType` or `type(None)`.
fn type_test_argument(compare: &ast::ExprCompare, holds: bool) -> Option<&Expr> {
    let (left, op, right) = compare.as_single()?;
    let Expr::Call(call) = left else {
        return None;
    };
    let (Expr::Name(function), [checked]) = (&*call.func, &*call.arguments.args) else {
        return None;
    };
    if function.id != "type" || !call.arguments.keywords.is_empty() || checked.is_starred_expr() {
        return None;
    }
    let is_class = |class: &Expr| match class {
        Expr::Name(name) => name.id != "NoneType",
        Expr::Attribute(attribute) => attribute.attr.as_str() != "NoneType",
        _ => false,
    };
    let classes_ok = match (op, holds) {
        (ast::CmpOp::Is | ast::CmpOp::Eq, true)
        | (ast::CmpOp::IsNot | ast::CmpOp::NotEq, false) => is_class(right),
        (ast::CmpOp::In, true) | (ast::CmpOp::NotIn, false) => match right {
            Expr::Set(ast::ExprSet { elts, .. })
            | Expr::Tuple(ast::ExprTuple { elts, .. })
            | Expr::List(ast::ExprList { elts, .. }) => elts.iter().all(is_class),
            _ => false,
        },
        _ => false,
    };
    classes_ok.then_some(checked)
}

/// The argument of `bool(X)`, which is true exactly when `X` is: `bool(a and b)` checks `a` and
/// `b`.
fn bool_argument(call: &ast::ExprCall) -> Option<&Expr> {
    match (&*call.func, &*call.arguments.args) {
        (Expr::Name(function), [value])
            if function.id == "bool" && call.arguments.keywords.is_empty() =>
        {
            Some(value)
        }
        _ => None,
    }
}

/// The owner and the member name of `getattr(owner, "name")` or `getattr(owner, "name", None)`,
/// which reads the same as `owner.name` but gives `None` where that would raise.
fn getattr_member(call: &ast::ExprCall) -> Option<(&Expr, Name)> {
    if !call
        .func
        .as_name_expr()
        .is_some_and(|function| function.id == "getattr")
        || !call.arguments.keywords.is_empty()
    {
        return None;
    }
    match &*call.arguments.args {
        [owner, Expr::StringLiteral(name)] => Some((owner, Name::new(name.value.to_str()))),
        [owner, Expr::StringLiteral(name), default] if default.is_none_literal_expr() => {
            Some((owner, Name::new(name.value.to_str())))
        }
        _ => None,
    }
}

/// The values of a string literal, or of a tuple, list or set display of them.
pub(super) fn string_literals(expression: &Expr) -> Option<Vec<Name>> {
    match expression {
        Expr::StringLiteral(literal) => Some(vec![Name::new(literal.value.to_str())]),
        Expr::Tuple(ast::ExprTuple { elts, .. })
        | Expr::List(ast::ExprList { elts, .. })
        | Expr::Set(ast::ExprSet { elts, .. }) => elts
            .iter()
            .map(|element| match element {
                Expr::StringLiteral(literal) => Some(Name::new(literal.value.to_str())),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// A local name assigned once, to a value that is a path from the context.
#[derive(Clone, Debug)]
struct Alias {
    path: Vec<Name>,
    /// The range of the assigned value.
    value: TextRange,
}

/// The path of `expression` from the context `context`: the context itself (`context`,
/// `bpy.context`) is the empty path, and an attribute read extends its receiver's.
fn path_of(
    expression: &Expr,
    context: &Name,
    aliases: &FxHashMap<Name, Alias>,
) -> Option<Vec<Name>> {
    match expression {
        Expr::Name(name) if name.id == *context => Some(Vec::new()),
        Expr::Name(name) => aliases.get(&name.id).map(|alias| alias.path.clone()),
        Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            if attr.id == "context" && value.as_name_expr().is_some_and(|value| value.id == "bpy") {
                return Some(Vec::new());
            }
            let mut path = path_of(value, context, aliases)?;
            path.push(attr.id.clone());
            Some(path)
        }
        // `(ob := context.object)` is `context.object`.
        Expr::Named(ast::ExprNamed { value, .. }) => path_of(value, context, aliases),
        Expr::Call(call) => {
            let (owner, name) = getattr_member(call)?;
            let mut path = path_of(owner, context, aliases)?;
            path.push(name);
            Some(path)
        }
        _ => None,
    }
}

/// The names of a function with `parameters` and `body` (or of a module, without parameters)
/// assigned exactly once, by `name = value`, to a path from `context`. A name bound any other way
/// (a parameter, a loop target, an augmented assignment, ...) or twice is no alias.
fn aliases(
    parameters: Option<&ast::Parameters>,
    body: &[Stmt],
    context: &Name,
) -> FxHashMap<Name, Alias> {
    let mut bindings = Bindings::default();
    for parameter in parameters
        .into_iter()
        .flat_map(|parameters| parameters.iter())
    {
        bindings.other(parameter.name().id.clone());
    }
    bindings.block(body);
    let values: FxHashMap<Name, &Expr> = bindings
        .assigned
        .into_iter()
        .filter(|(name, values)| values.len() == 1 && !bindings.others.contains(name))
        .filter_map(|(name, values)| Some((name, *values.first()?)))
        .collect();
    let mut aliases = FxHashMap::default();
    for (name, value) in &values {
        if let Some(path) = alias_path(value, context, &values, 0) {
            aliases.insert(
                name.clone(),
                Alias {
                    path,
                    value: value.range(),
                },
            );
        }
    }
    aliases
}

fn alias_path(
    expression: &Expr,
    context: &Name,
    values: &FxHashMap<Name, &Expr>,
    depth: usize,
) -> Option<Vec<Name>> {
    if depth > MAX_ALIAS_DEPTH {
        return None;
    }
    match expression {
        Expr::Name(name) if name.id == *context => Some(Vec::new()),
        Expr::Name(name) => alias_path(values.get(&name.id)?, context, values, depth + 1),
        Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            if attr.id == "context" && value.as_name_expr().is_some_and(|value| value.id == "bpy") {
                return Some(Vec::new());
            }
            let mut path = alias_path(value, context, values, depth)?;
            path.push(attr.id.clone());
            Some(path)
        }
        Expr::Named(ast::ExprNamed { value, .. }) => alias_path(value, context, values, depth),
        Expr::Call(call) => {
            let (owner, name) = getattr_member(call)?;
            let mut path = alias_path(owner, context, values, depth)?;
            path.push(name);
            Some(path)
        }
        _ => None,
    }
}

/// The walrus targets in an expression, with their values.
struct NamedTargets<'a>(Vec<(&'a Name, &'a Expr)>);

impl<'a> SourceOrderVisitor<'a> for NamedTargets<'a> {
    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Named(ast::ExprNamed { target, value, .. }) => {
                if let Expr::Name(name) = &**target {
                    self.0.push((&name.id, value));
                }
                self.visit_expr(value);
            }
            Expr::Lambda(_)
            | Expr::ListComp(_)
            | Expr::SetComp(_)
            | Expr::DictComp(_)
            | Expr::Generator(_) => {}
            _ => walk_expr(self, expression),
        }
    }
}

/// The bindings of a function body, outside nested functions and classes.
#[derive(Default)]
struct Bindings<'a> {
    assigned: FxHashMap<Name, Vec<&'a Expr>>,
    others: Vec<Name>,
}

impl<'a> Bindings<'a> {
    fn other(&mut self, name: Name) {
        self.others.push(name);
    }

    fn target(&mut self, target: &Expr) {
        match target {
            Expr::Name(name) => self.other(name.id.clone()),
            Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
                for element in elts {
                    self.target(element);
                }
            }
            Expr::Starred(ast::ExprStarred { value, .. }) => self.target(value),
            _ => {}
        }
    }

    /// Binds the targets of the walrus expressions in `expression`, outside lambdas and
    /// comprehensions: `(ob := context.object)` assigns `ob`.
    fn named(&mut self, expression: &'a Expr) {
        let mut visitor = NamedTargets(Vec::new());
        visitor.visit_expr(expression);
        for (name, value) in visitor.0 {
            self.assigned.entry(name.clone()).or_default().push(value);
        }
    }

    fn block(&mut self, body: &'a [Stmt]) {
        for statement in body {
            match statement {
                Stmt::Return(ast::StmtReturn {
                    value: Some(value), ..
                })
                | Stmt::Expr(ast::StmtExpr { value, .. })
                | Stmt::Assign(ast::StmtAssign { value, .. })
                | Stmt::AugAssign(ast::StmtAugAssign { value, .. })
                | Stmt::AnnAssign(ast::StmtAnnAssign {
                    value: Some(value), ..
                }) => self.named(value),
                Stmt::If(ast::StmtIf {
                    test,
                    elif_else_clauses,
                    ..
                }) => {
                    self.named(test);
                    for clause in elif_else_clauses {
                        if let Some(test) = &clause.test {
                            self.named(test);
                        }
                    }
                }
                Stmt::While(ast::StmtWhile { test, .. })
                | Stmt::Assert(ast::StmtAssert { test, .. }) => self.named(test),
                _ => {}
            }
            match statement {
                Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
                    for target in targets {
                        if let Expr::Name(name) = target {
                            self.assigned
                                .entry(name.id.clone())
                                .or_default()
                                .push(value);
                        } else {
                            self.target(target);
                        }
                    }
                }
                Stmt::AnnAssign(ast::StmtAnnAssign { target, value, .. }) => {
                    match (&**target, value) {
                        (Expr::Name(name), Some(value)) => {
                            self.assigned
                                .entry(name.id.clone())
                                .or_default()
                                .push(value);
                        }
                        _ => self.target(target),
                    }
                }
                Stmt::AugAssign(ast::StmtAugAssign { target, .. }) => self.target(target),
                Stmt::Delete(ast::StmtDelete { targets, .. }) => {
                    for target in targets {
                        self.target(target);
                    }
                }
                Stmt::For(statement) => {
                    self.target(&statement.target);
                    self.block(&statement.body);
                    self.block(&statement.orelse);
                }
                Stmt::While(statement) => {
                    self.block(&statement.body);
                    self.block(&statement.orelse);
                }
                Stmt::If(statement) => {
                    self.block(&statement.body);
                    for clause in &statement.elif_else_clauses {
                        self.block(&clause.body);
                    }
                }
                Stmt::With(statement) => {
                    for item in &statement.items {
                        if let Some(target) = &item.optional_vars {
                            self.target(target);
                        }
                    }
                    self.block(&statement.body);
                }
                Stmt::Try(statement) => {
                    self.block(&statement.body);
                    for handler in &statement.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        if let Some(name) = &handler.name {
                            self.other(name.id.clone());
                        }
                        self.block(&handler.body);
                    }
                    self.block(&statement.orelse);
                    self.block(&statement.finalbody);
                }
                Stmt::Match(statement) => {
                    for case in &statement.cases {
                        self.block(&case.body);
                    }
                }
                Stmt::FunctionDef(ast::StmtFunctionDef { name, .. })
                | Stmt::ClassDef(ast::StmtClassDef { name, .. }) => self.other(name.id.clone()),
                Stmt::Import(ast::StmtImport { names, .. })
                | Stmt::ImportFrom(ast::StmtImportFrom { names, .. }) => {
                    for alias in names {
                        let name = alias.asname.as_ref().unwrap_or(&alias.name);
                        self.other(name.id.clone());
                    }
                }
                Stmt::Global(ast::StmtGlobal { names, .. })
                | Stmt::Nonlocal(ast::StmtNonlocal { names, .. }) => {
                    for name in names {
                        self.other(name.id.clone());
                    }
                }
                _ => {}
            }
        }
    }
}

/// The paths from the context in a Blender callback, and the reads it only tests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) struct FunctionPaths {
    /// The callback's context parameter (the second one).
    context: Option<Name>,
    /// The single-assignment aliases of paths.
    aliases: Box<[(Name, Path)]>,
    /// The reads in a test for truth or `None` (`if context.object:`, `x is None`), also the value
    /// of an alias that is tested so (`ob = context.object` with `if ob is None:` after it).
    tested: Box<[TextRange]>,
    /// Where the callback's own test says a path is not `None`: the body of `if X:` or `if X is
    /// not None:`, and the rest of the block after `if not X: return` or `assert X`.
    regions: Box<[(Path, TextRange)]>,
}

impl FunctionPaths {
    /// The path of the attribute read `attribute` in the callback.
    pub(super) fn attribute_path(&self, attribute: &ast::ExprAttribute) -> Option<Vec<Name>> {
        let context = self.context.as_ref()?;
        let mut path = self.path(&attribute.value, context)?;
        path.push(attribute.attr.id.clone());
        Some(path)
    }

    fn path(&self, expression: &Expr, context: &Name) -> Option<Vec<Name>> {
        match expression {
            Expr::Name(name) if name.id == *context => Some(Vec::new()),
            Expr::Name(name) => self
                .aliases
                .iter()
                .find(|(alias, _)| *alias == name.id)
                .map(|(_, path)| path.to_vec()),
            Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                if attr.id == "context"
                    && value.as_name_expr().is_some_and(|value| value.id == "bpy")
                {
                    return Some(Vec::new());
                }
                let mut path = self.path(value, context)?;
                path.push(attr.id.clone());
                Some(path)
            }
            _ => None,
        }
    }

    /// Whether the read of `path` at `range` is one the callback only tests, or one inside a
    /// region where its own test already narrows the path.
    fn skips(&self, range: TextRange, path: &[Name]) -> bool {
        self.tested.contains(&range)
            || self
                .regions
                .iter()
                .any(|(region_path, region)| **region_path == *path && region.contains_range(range))
    }
}

/// Whether the attribute read `attribute`, in the function scope `function`, is one the function
/// only tests for truth or `None`, or one its own test already narrows.
pub(super) fn is_tested_read<'db>(
    db: &'db dyn Db,
    function: ScopeId<'db>,
    attribute: &ast::ExprAttribute,
) -> bool {
    let paths = function_paths(db, function);
    paths
        .attribute_path(attribute)
        .is_some_and(|path| paths.skips(attribute.range(), &path))
}

/// The paths and tested reads of the scope `scope`: a function, whose context is its first
/// parameter annotated `Context` or else its second one (a callback's), or a module, where only
/// `bpy.context` is the context. A `poll`'s returned value is a test too.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(super) fn function_paths<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> FunctionPaths {
    let file: ProgramFile<'db> = scope.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let node = scope.node(db);
    let mut is_poll = false;
    let (parameters, body, context) = if let Some(function) = node.as_function() {
        let function = function.node(&module);
        is_poll = function.name.id == "poll";
        let parameters = &function.parameters;
        let mut positional = parameters.posonlyargs.iter().chain(&parameters.args);
        let annotated = parameters.iter().find(|parameter| {
            parameter
                .annotation()
                .is_some_and(|annotation| match annotation {
                    Expr::Name(name) => name.id == "Context",
                    Expr::Attribute(attribute) => attribute.attr.id == "Context",
                    Expr::StringLiteral(string) => string.value.to_str().ends_with("Context"),
                    _ => false,
                })
        });
        let context = match annotated {
            Some(parameter) => parameter.name().id.clone(),
            None => match positional.nth(1) {
                Some(parameter) => parameter.parameter.name.id.clone(),
                None => Name::new_static(""),
            },
        };
        (Some(&*function.parameters), &function.body[..], context)
    } else if scope.file_scope_id(db).is_global() {
        (None, &module.suite()[..], Name::new_static(""))
    } else {
        return FunctionPaths::default();
    };
    let aliases = aliases(parameters, body, &context);
    let mut tests = Tests {
        context: &context,
        aliases: &aliases,
        tested: Vec::new(),
        regions: Vec::new(),
        own_reads: false,
        returns_test: is_poll,
    };
    tests.block(body);
    let Tests {
        tested, regions, ..
    } = tests;
    FunctionPaths {
        aliases: aliases
            .into_iter()
            .map(|(name, alias)| (name, alias.path.into_boxed_slice()))
            .collect(),
        context: Some(context),
        tested: tested.into_boxed_slice(),
        regions: regions.into_boxed_slice(),
    }
}

/// The reads of the local name `name` in `body` that are only tested for truth or `None`, and the
/// ranges where `body`'s own test says `name` is not `None`: the body of `if name:` or
/// `if name is not None:`, and the rest of the block after `if name is None: return`.
pub(super) fn name_tests(body: &[Stmt], name: &Name) -> (Vec<TextRange>, Vec<TextRange>) {
    let mut aliases = FxHashMap::default();
    aliases.insert(
        name.clone(),
        Alias {
            path: vec![name.clone()],
            value: TextRange::default(),
        },
    );
    let context = Name::new_static("");
    let mut tests = Tests {
        context: &context,
        aliases: &aliases,
        tested: Vec::new(),
        regions: Vec::new(),
        own_reads: true,
        returns_test: false,
    };
    tests.block(body);
    let regions = tests
        .regions
        .into_iter()
        .filter(|(path, _)| **path == [name.clone()])
        .map(|(_, range)| range)
        .collect();
    (tests.tested, regions)
}

/// Finds the tests of a callback body for truth or `None`.
struct Tests<'a> {
    context: &'a Name,
    aliases: &'a FxHashMap<Name, Alias>,
    tested: Vec<TextRange>,
    regions: Vec<(Path, TextRange)>,
    /// Whether a tested alias records the read of the name itself rather than its value.
    own_reads: bool,
    /// Whether the returned value is a test, as a `poll`'s is.
    returns_test: bool,
}

impl Tests<'_> {
    fn path(&self, expression: &Expr) -> Option<Path> {
        path_of(expression, self.context, self.aliases)
            .filter(|path| !path.is_empty())
            .map(Vec::into_boxed_slice)
    }

    fn block(&mut self, body: &[Stmt]) {
        let block_end = body.last().map(Ranged::end);
        for statement in body {
            match statement {
                Stmt::If(if_statement) => {
                    self.test(&if_statement.test);
                    if let (Some(first), Some(last)) =
                        (if_statement.body.first(), if_statement.body.last())
                    {
                        let range = TextRange::new(first.start(), last.end());
                        for path in self.truthy_paths(&if_statement.test) {
                            self.regions.push((path, range));
                        }
                    }
                    if if_statement.elif_else_clauses.is_empty()
                        && if_statement.body.last().is_some_and(exits)
                        && let Some(end) = block_end
                        && statement.end() < end
                    {
                        let range = TextRange::new(statement.end(), end);
                        for path in self.falsy_paths(&if_statement.test) {
                            self.regions.push((path, range));
                        }
                    }
                    self.block(&if_statement.body);
                    for clause in &if_statement.elif_else_clauses {
                        if let Some(test) = &clause.test {
                            self.test(test);
                        }
                        self.block(&clause.body);
                    }
                }
                Stmt::While(statement) => {
                    self.test(&statement.test);
                    self.block(&statement.body);
                    self.block(&statement.orelse);
                }
                Stmt::Assert(statement) => {
                    self.test(&statement.test);
                    if let Some(end) = block_end
                        && statement.end() < end
                    {
                        let range = TextRange::new(statement.end(), end);
                        for path in self.truthy_paths(&statement.test) {
                            self.regions.push((path, range));
                        }
                    }
                }
                Stmt::For(statement) => {
                    self.block(&statement.body);
                    self.block(&statement.orelse);
                }
                Stmt::With(statement) => self.block(&statement.body),
                Stmt::Return(ast::StmtReturn {
                    value: Some(value), ..
                }) if self.returns_test => self.test(value),
                Stmt::Try(statement) => {
                    self.block(&statement.body);
                    for handler in &statement.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        self.block(&handler.body);
                    }
                    self.block(&statement.orelse);
                    self.block(&statement.finalbody);
                }
                Stmt::Match(statement) => {
                    for case in &statement.cases {
                        self.block(&case.body);
                    }
                }
                _ => {}
            }
        }
    }

    /// Records the reads `test` tests for truth or `None`.
    fn test(&mut self, test: &Expr) {
        match test {
            Expr::BoolOp(ast::ExprBoolOp { values, .. }) => {
                for value in values {
                    self.test(value);
                }
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => self.test(operand),
            Expr::Compare(compare) => {
                if let Some((operand, op, other)) = single_comparison(compare)
                    && other.is_none_literal_expr()
                    && matches!(
                        op,
                        ast::CmpOp::Is | ast::CmpOp::IsNot | ast::CmpOp::Eq | ast::CmpOp::NotEq
                    )
                {
                    self.tested_read(operand);
                } else if let Some(checked) =
                    type_test_argument(compare, true).or_else(|| type_test_argument(compare, false))
                {
                    self.tested_read(checked);
                }
            }
            Expr::Call(call) => {
                if let Some(argument) = checked_argument(call) {
                    self.tested_read(argument);
                }
            }
            Expr::Name(_) | Expr::Attribute(_) => self.tested_read(test),
            _ => {}
        }
    }

    fn tested_read(&mut self, expression: &Expr) {
        match expression {
            Expr::Attribute(_) => self.tested.push(expression.range()),
            Expr::Name(name) => {
                if let Some(alias) = self.aliases.get(&name.id) {
                    self.tested.push(if self.own_reads {
                        name.range
                    } else {
                        alias.value
                    });
                }
            }
            _ => {}
        }
    }

    /// The paths that are not `None` when `test` is true.
    fn truthy_paths(&self, test: &Expr) -> Vec<Path> {
        match test {
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::And,
                values,
                ..
            }) => values
                .iter()
                .flat_map(|value| self.truthy_paths(value))
                .collect(),
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => self.falsy_paths(operand),
            Expr::Compare(compare) => match single_comparison(compare) {
                Some((operand, ast::CmpOp::IsNot | ast::CmpOp::NotEq, other))
                    if other.is_none_literal_expr() =>
                {
                    self.path(operand).into_iter().collect()
                }
                _ => type_test_argument(compare, true)
                    .and_then(|checked| self.path(checked))
                    .into_iter()
                    .collect(),
            },
            Expr::Call(call) => checked_argument(call)
                .and_then(|checked| self.path(checked))
                .into_iter()
                .collect(),
            Expr::Name(_) | Expr::Attribute(_) => self.path(test).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    /// The paths that are not `None` when `test` is false.
    fn falsy_paths(&self, test: &Expr) -> Vec<Path> {
        match test {
            Expr::BoolOp(ast::ExprBoolOp {
                op: ast::BoolOp::Or,
                values,
                ..
            }) => values
                .iter()
                .flat_map(|value| self.falsy_paths(value))
                .collect(),
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => self.truthy_paths(operand),
            Expr::Compare(compare) => match single_comparison(compare) {
                Some((operand, ast::CmpOp::Is | ast::CmpOp::Eq, other))
                    if other.is_none_literal_expr() =>
                {
                    self.path(operand).into_iter().collect()
                }
                _ => type_test_argument(compare, false)
                    .and_then(|checked| self.path(checked))
                    .into_iter()
                    .collect(),
            },
            _ => Vec::new(),
        }
    }
}

/// Whether `statement` leaves its block: `return`, `raise`, `continue` or `break`.
pub(super) fn exits(statement: &Stmt) -> bool {
    matches!(
        statement,
        Stmt::Return(_) | Stmt::Raise(_) | Stmt::Continue(_) | Stmt::Break(_)
    )
}
