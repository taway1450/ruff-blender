//! The registration checks: what Blender refuses, ignores or leaves behind when it runs an add-on's
//! `register()` and `unregister()`, read from the two walks ([`super::walk`]).
//!
//! A check that says something is missing (a class never registered, a function never called, a
//! class never unregistered) needs the whole of a walk, and stays silent when the walk gave up. A
//! check about two events (one class registered before another, twice, or with the idname of
//! another) needs both events to be exact: a timer's or a handler's events have no known order.
//!
//! The findings of an add-on are computed once and reported by the per-file query of each file
//! they are in; findings in files outside the add-on (a library it calls) are dropped.

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashSet;
use ty_blender::{IdnameFamily, OperatorIdname};
use ty_python_core::definition::{Definition, DefinitionKind, DefinitionState};
use ty_python_core::{place_table, use_def_map};

use crate::Db;
use crate::types::class_base::ClassBase;
use crate::types::{
    LiteralValueType, StaticClassLiteral, Type, binding_type, definition_expression_type_in_scope,
};

use super::addon::is_first_party;
use super::evaluate::{PropertyValue, property_value};
use super::index::{AddonInProgram, file_facts};
use super::properties::PointerTarget;
use super::symbols::{bpy_types_bases, is_bpy_types_class};
use super::walk::{
    BranchPath, ClassEvent, Event, HookTarget, Site, Walk, register_walk, registration_ir,
    unregister_walk,
};

/// The registration rules, each reported by its own lint.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) enum Rule {
    RegisterOrder,
    DuplicateRegistration,
    InvalidIdname,
    UnregisteredClass,
    UnreachableRegistration,
    UnregisterMismatch,
}

/// One registration mistake, at a place in one of the add-on's files.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct Finding {
    pub(super) rule: Rule,
    pub(super) file: File,
    pub(super) range: TextRange,
    pub(super) message: String,
}

/// The registration mistakes of an add-on, at most one per rule and place.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn registration_findings<'db>(
    db: &'db dyn Db,
    key: AddonInProgram<'db>,
) -> Box<[Finding]> {
    let register = register_walk(db, key);
    let unregister = unregister_walk(db, key);
    let mut checks = Checks {
        db,
        files: key.files(db).map(|file| file.file(db)).collect(),
        findings: Vec::new(),
    };
    let classes = AddonClasses::new(db, key);
    checks.unregistered_and_unreachable(key, register, unregister, &classes);
    checks.register_order(register, &classes);
    checks.duplicates(register);
    checks.idnames(register, &classes);
    checks.unregister_mismatch(register, unregister);
    let mut seen = FxHashSet::default();
    checks
        .findings
        .into_iter()
        .filter(|finding| seen.insert((finding.rule, finding.file, finding.range)))
        .collect()
}

/// The module-level classes of an add-on's files that `register_class` accepts.
struct AddonClasses<'db> {
    /// Each class with the file and the range of its name.
    classes: Vec<(StaticClassLiteral<'db>, Site)>,
    /// The classes that another of them derives from. Blender never needs them registered: a
    /// registered subclass copies what it inherits.
    bases: FxHashSet<StaticClassLiteral<'db>>,
}

impl<'db> AddonClasses<'db> {
    fn new(db: &'db dyn Db, key: AddonInProgram<'db>) -> Self {
        let mut classes = Vec::new();
        for file in key.files(db) {
            for class in &file_facts(db, file).classes {
                classes.push((
                    class.class,
                    Site {
                        file: file.file(db),
                        range: class.name_range,
                    },
                ));
            }
        }
        let all: FxHashSet<_> = classes.iter().map(|(class, _)| *class).collect();
        let mut bases = FxHashSet::default();
        for (class, _) in &classes {
            for base in mro_classes(db, *class).skip(1) {
                if all.contains(&base) {
                    bases.insert(base);
                }
            }
        }
        AddonClasses { classes, bases }
    }
}

/// The static classes of the method resolution order of `class`, starting with `class` itself.
fn mro_classes<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> impl Iterator<Item = StaticClassLiteral<'db>> {
    class.iter_mro(db, None).filter_map(move |base| match base {
        ClassBase::Class(class_type) => class_type.class_literal(db).as_static(),
        _ => None,
    })
}

struct Checks<'db> {
    db: &'db dyn Db,
    files: FxHashSet<File>,
    findings: Vec<Finding>,
}

impl<'db> Checks<'db> {
    fn report(&mut self, rule: Rule, site: Site, message: String) {
        if self.files.contains(&site.file) {
            self.findings.push(Finding {
                rule,
                file: site.file,
                range: site.range,
                message,
            });
        }
    }

    /// `blender-unregistered-class` and `blender-unreachable-registration`. A class that only an
    /// unreachable function registers is reported once, at the function.
    fn unregistered_and_unreachable(
        &mut self,
        key: AddonInProgram<'db>,
        register: &Walk<'db>,
        unregister: &Walk<'db>,
        classes: &AddonClasses<'db>,
    ) {
        let db = self.db;
        if !register.is_complete() {
            return;
        }
        let entered: FxHashSet<Definition<'db>> = register
            .entered
            .iter()
            .chain(&unregister.entered)
            .copied()
            .collect();
        let mut unreachable_classes = FxHashSet::default();
        for file in key.files(db) {
            for function in registration_ir(db, file).registering_functions(db) {
                if entered.contains(&function.definition) {
                    continue;
                }
                unreachable_classes.extend(function.named_classes.iter().copied());
                let name = function.definition.name(db).unwrap_or_default();
                self.report(
                    Rule::UnreachableRegistration,
                    Site {
                        file: file.file(db),
                        range: function.name_range,
                    },
                    format!("`{name}` registers classes, but `register()` never calls it"),
                );
            }
        }
        let registered: FxHashSet<_> = registered_classes(register).collect();
        for (class, site) in &classes.classes {
            if registered.contains(class)
                || classes.bases.contains(class)
                || unreachable_classes.contains(class)
            {
                continue;
            }
            let name = class.name(db);
            self.report(
                Rule::UnregisteredClass,
                *site,
                format!(
                    "Blender class `{name}` is never registered: `register()` does not reach a `register_class({name})`"
                ),
            );
        }
    }

    /// `blender-register-order`: a class or a Registered property that points to a
    /// `PropertyGroup` not registered yet, and a subpanel registered before its parent.
    fn register_order(&mut self, register: &Walk<'db>, classes: &AddonClasses<'db>) {
        let db = self.db;
        let events = &register.events;
        for (index, event) in events.iter().enumerate() {
            match event {
                Event::RegisterClass(registration) if registration.exact => {
                    for (target, leaf) in pointer_targets(db, registration.class) {
                        if let Some(problem) =
                            order_problem(register, index, &registration.path, target)
                        {
                            let class = registration.class.name(db);
                            let target_name = target.name(db);
                            self.report(
                                Rule::RegisterOrder,
                                registration.named_at,
                                format!(
                                    "`{class}` points to `{target_name}` (`{leaf}`), which {problem}; register `{target_name}` first"
                                ),
                            );
                        }
                    }
                    if let Some(parent_id) = class_string(db, registration.class, "bl_parent_id")
                        && let Some(parent) = classes.classes.iter().find_map(|(class, _)| {
                            (bpy_types_bases(db, *class).any(|base| base == "Panel")
                                && class_idname(db, *class).as_deref() == Some(parent_id.as_str()))
                            .then_some(*class)
                        })
                        && let Some(problem) =
                            order_problem(register, index, &registration.path, parent)
                    {
                        let class = registration.class.name(db);
                        let parent_name = parent.name(db);
                        self.report(
                            Rule::RegisterOrder,
                            registration.named_at,
                            format!(
                                "The subpanel `{class}` has the parent `{parent_name}`, which {problem}; register `{parent_name}` first"
                            ),
                        );
                    }
                }
                Event::SetProperty {
                    property,
                    site,
                    path,
                    exact: true,
                } => {
                    let PropertyValue::Declarations(declarations) = &property.value else {
                        continue;
                    };
                    for declaration in declarations {
                        let Some((target, PointerTarget::PropertyGroup)) =
                            declaration.target_class(db)
                        else {
                            continue;
                        };
                        if !super::index::is_registrable(db, target) {
                            continue;
                        }
                        if let Some(problem) = order_problem(register, index, path, target) {
                            let owner = property.owner.name(db);
                            let name = &property.name;
                            let target_name = target.name(db);
                            self.report(
                                Rule::RegisterOrder,
                                *site,
                                format!(
                                    "`{owner}.{name}` points to `{target_name}`, which {problem}; register `{target_name}` first"
                                ),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// `blender-duplicate-registration`: a class registered again while it is registered, and two
    /// classes of one idname family with the same idname.
    fn duplicates(&mut self, register: &Walk<'db>) {
        let db = self.db;
        let class_events: Vec<(bool, &ClassEvent<'db>)> = register
            .events
            .iter()
            .filter_map(|event| match event {
                Event::RegisterClass(event) => Some((true, event)),
                Event::UnregisterClass(event) => Some((false, event)),
                _ => None,
            })
            .collect();
        for (index, (registers, event)) in class_events.iter().enumerate() {
            if !registers || !event.exact {
                continue;
            }
            // The latest earlier event of the same class that can happen along with this one.
            let previous = class_events[..index].iter().rev().find(|(_, earlier)| {
                earlier.class == event.class && !earlier.path.is_exclusive_with(&event.path)
            });
            if let Some((true, earlier)) = previous
                && earlier.exact
            {
                let name = event.class.name(db);
                self.report(
                    Rule::DuplicateRegistration,
                    event.named_at,
                    format!(
                        "`{name}` is registered again while it is registered; Blender refuses it"
                    ),
                );
            }
        }
        // Two classes with one idname: the later one replaces the earlier.
        let mut registered: Vec<(&ClassEvent<'db>, IdnameFamily, String)> = Vec::new();
        for (registers, event) in &class_events {
            if !registers || !event.exact {
                continue;
            }
            let Some(family) = idname_family(db, event.class) else {
                continue;
            };
            let Some(idname) = family_idname(db, event.class, family) else {
                continue;
            };
            if let Some((earlier, _, _)) =
                registered.iter().find(|(earlier, other, other_idname)| {
                    earlier.class != event.class
                        && *other == family
                        && *other_idname == idname
                        && !earlier.path.is_exclusive_with(&event.path)
                })
            {
                let name = event.class.name(db);
                let earlier_name = earlier.class.name(db);
                {
                    self.report(
                        Rule::DuplicateRegistration,
                        idname_site(db, event.class),
                        format!(
                            "`{name}` has the idname `\"{idname}\"` of `{earlier_name}`, registered before it; Blender replaces `{earlier_name}`"
                        ),
                    );
                }
            }
            registered.push((event, family, idname));
        }
    }

    /// `blender-invalid-idname`, on the registered classes; when the walk gave up, on every class
    /// that may be registered.
    fn idnames(&mut self, register: &Walk<'db>, classes: &AddonClasses<'db>) {
        let db = self.db;
        let registered: FxHashSet<_> = registered_classes(register).collect();
        for (class, class_site) in &classes.classes {
            let checked = registered.contains(class)
                || (!register.is_complete() && !classes.bases.contains(class));
            if !checked {
                continue;
            }
            let Some(family) = idname_family(db, *class) else {
                continue;
            };
            let name = class.name(db);
            match (family, class_idname_binding(db, *class)) {
                (_, Idname::NotLiteral) => {}
                (IdnameFamily::Operator, Idname::Missing) => self.report(
                    Rule::InvalidIdname,
                    *class_site,
                    format!(
                        "`{name}` has no `bl_idname`; Blender refuses an operator without a `category.name` idname"
                    ),
                ),
                (IdnameFamily::Struct(_), Idname::Missing) => {
                    if let Some(problem) = family.problem(name) {
                        self.report(
                            Rule::InvalidIdname,
                            *class_site,
                            format!(
                                "`{name}` has no `bl_idname`, so its class name is its idname: {problem}"
                            ),
                        );
                    }
                }
                (_, Idname::Literal(idname, site)) => {
                    if let Some(problem) = family.problem(&idname) {
                        // An idname inherited from a class outside the add-on is reported at the class.
                        let site = if self.files.contains(&site.file) {
                            site
                        } else {
                            *class_site
                        };
                        self.report(
                            Rule::InvalidIdname,
                            site,
                            format!("The idname of `{name}` is invalid: {problem}"),
                        );
                    }
                }
            }
        }
    }

    /// `blender-unregister-mismatch`: what `register()` adds and `unregister()` leaves behind, and
    /// a class `unregister()` unregisters that `register()` never registers.
    fn unregister_mismatch(&mut self, register: &Walk<'db>, unregister: &Walk<'db>) {
        let db = self.db;
        if unregister.is_complete() {
            let mut seen_classes = FxHashSet::default();
            for event in &register.events {
                match event {
                    Event::RegisterClass(registration) => {
                        if !seen_classes.insert(registration.class) {
                            continue;
                        }
                        let unregistered = unregister.events.iter().any(|event| {
                            matches!(event, Event::UnregisterClass(other) if other.class == registration.class)
                        });
                        if !unregistered {
                            let name = registration.class.name(db);
                            self.report(
                                Rule::UnregisterMismatch,
                                registration.named_at,
                                format!(
                                    "`{name}` is registered but `unregister()` never unregisters it"
                                ),
                            );
                        }
                    }
                    Event::SetProperty { property, site, .. } if property.accepted => {
                        let deleted = unregister.events.iter().any(|event| {
                            matches!(event, Event::DelProperty { owner, name }
                                if *owner == property.owner && *name == property.name)
                        });
                        if !deleted {
                            let owner = property.owner.name(db);
                            let name = &property.name;
                            self.report(
                                Rule::UnregisterMismatch,
                                *site,
                                format!(
                                    "`{owner}.{name}` is added in `register()` but `unregister()` never deletes it"
                                ),
                            );
                        }
                    }
                    Event::Hook {
                        target,
                        function,
                        added: true,
                        site,
                    } => {
                        let removed = unregister.events.iter().any(|event| {
                            matches!(event, Event::Hook { target: other, function: other_function, added: false, .. }
                                if other == target && other_function == function)
                        });
                        if !removed {
                            let name = function.name(db).unwrap_or_default();
                            let target = hook_target_name(db, target);
                            self.report(
                                Rule::UnregisterMismatch,
                                *site,
                                format!(
                                    "`{name}` is added to `{target}` in `register()` but `unregister()` never removes it"
                                ),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        if register.is_complete() {
            let registered: FxHashSet<_> = registered_classes(register).collect();
            for event in &unregister.events {
                if let Event::UnregisterClass(unregistration) = event
                    && !registered.contains(&unregistration.class)
                {
                    let name = unregistration.class.name(db);
                    self.report(
                        Rule::UnregisterMismatch,
                        unregistration.named_at,
                        format!(
                            "`unregister()` unregisters `{name}`, which `register()` never registers; Blender refuses it"
                        ),
                    );
                }
            }
        }
    }
}

/// Why `target` is not registered when the event at `index` of `register` needs it, or `None` when
/// it is or the walk can't tell.
fn order_problem<'db>(
    register: &Walk<'db>,
    index: usize,
    path: &BranchPath,
    target: StaticClassLiteral<'db>,
) -> Option<&'static str> {
    if register.events[..index]
        .iter()
        .filter_map(|event| registration_of(event, target))
        .any(|earlier| !earlier.path.is_exclusive_with(path))
    {
        return None;
    }
    let mut later = register.events[index + 1..]
        .iter()
        .filter_map(|event| registration_of(event, target));
    if later
        .clone()
        .any(|later| later.exact && !later.path.is_exclusive_with(path))
    {
        return Some("is registered after it");
    }
    (register.is_complete() && later.next().is_none()).then_some("is never registered")
}

/// The registration of `class` that `event` is, if it is one.
fn registration_of<'a, 'db>(
    event: &'a Event<'db>,
    class: StaticClassLiteral<'db>,
) -> Option<&'a ClassEvent<'db>> {
    match event {
        Event::RegisterClass(registration) if registration.class == class => Some(registration),
        _ => None,
    }
}

fn registered_classes<'a, 'db>(
    walk: &'a Walk<'db>,
) -> impl Iterator<Item = StaticClassLiteral<'db>> + 'a {
    walk.events.iter().filter_map(|event| match event {
        Event::RegisterClass(event) => Some(event.class),
        _ => None,
    })
}

fn hook_target_name(db: &dyn Db, target: &HookTarget<'_>) -> String {
    match target {
        HookTarget::Class(class) => class.name(db).to_string(),
        HookTarget::Path(path) => path.to_string(),
    }
}

/// The `PropertyGroup` classes of the add-on that the annotated properties of `class` point to
/// (its own and those of the first-party classes it derives from), with the property names.
fn pointer_targets<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Vec<(StaticClassLiteral<'db>, Name)> {
    let mut targets: Vec<(StaticClassLiteral<'db>, Name)> = Vec::new();
    for base in mro_classes(db, class) {
        if is_bpy_types_class(db, base) || !is_first_party(db, base.program_file(db)) {
            continue;
        }
        let scope = base.body_scope(db);
        let table = place_table(db, scope);
        for (symbol, declarations) in use_def_map(db, scope).all_end_of_scope_symbol_declarations()
        {
            for declaration in declarations {
                let DefinitionState::Defined(definition) = declaration.declaration else {
                    continue;
                };
                let Some(PropertyValue::Declarations(values)) = property_value(db, definition)
                else {
                    continue;
                };
                for value in values {
                    if let Some((target, PointerTarget::PropertyGroup)) = value.target_class(db)
                        && target != class
                        && super::index::is_registrable(db, target)
                        && !targets.iter().any(|(other, _)| *other == target)
                    {
                        targets.push((target, table.symbol(symbol).name().clone()));
                    }
                }
            }
        }
    }
    targets
}

/// The idname family of a class: operators and macros, or panels, menus, headers and UI lists.
fn idname_family(db: &dyn Db, class: StaticClassLiteral<'_>) -> Option<IdnameFamily> {
    bpy_types_bases(db, class).find_map(IdnameFamily::of_root)
}

/// The idname a class registers under in its family: its `bl_idname`, or for a panel, menu, header
/// or UI list without one, its class name. `None` when it can't be read.
fn family_idname(
    db: &dyn Db,
    class: StaticClassLiteral<'_>,
    family: IdnameFamily,
) -> Option<String> {
    match (class_idname_binding(db, class), family) {
        (Idname::Literal(idname, _), _) => Some(idname),
        (Idname::Missing, IdnameFamily::Struct(_)) => Some(class.name(db).to_string()),
        _ => None,
    }
}

/// The `bl_idname` of `class` with the class name as the default, as Blender reads a panel's.
fn class_idname(db: &dyn Db, class: StaticClassLiteral<'_>) -> Option<String> {
    match class_idname_binding(db, class) {
        Idname::Literal(idname, _) => Some(idname),
        Idname::Missing => Some(class.name(db).to_string()),
        Idname::NotLiteral => None,
    }
}

/// Where the duplicate idname of `class` is reported: its own `bl_idname`, else its class name.
fn idname_site(db: &dyn Db, class: StaticClassLiteral<'_>) -> Site {
    let definition = own_binding(db, class, "bl_idname")
        .map_or_else(|| class.definition(db), |(definition, _)| definition);
    definition_site(db, definition)
}

/// The name a definition binds: the `class` or `def` name, or the assignment target.
fn definition_site(db: &dyn Db, definition: Definition<'_>) -> Site {
    let module = parsed_module(db, definition.python_file(db)).load(db);
    Site {
        file: definition.file(db),
        range: definition.focus_range(db, &module).range(),
    }
}

/// The `bl_idname` of a class, as Blender reads it from the class and the classes it derives from.
enum Idname {
    /// A string literal, with its binding.
    Literal(String, Site),
    /// Something else (`__package__`, a call): not checked.
    NotLiteral,
    /// No class of the add-on sets one.
    Missing,
}

fn class_idname_binding(db: &dyn Db, class: StaticClassLiteral<'_>) -> Idname {
    match class_attribute(db, class, "bl_idname") {
        None => Idname::Missing,
        Some((_, None)) => Idname::NotLiteral,
        Some((definition, Some(value))) => Idname::Literal(value, definition_site(db, definition)),
    }
}

/// The string value of the class attribute `name`, when it is a string literal.
pub(super) fn class_string(
    db: &dyn Db,
    class: StaticClassLiteral<'_>,
    name: &str,
) -> Option<String> {
    class_attribute(db, class, name)?.1
}

/// The last binding of the class attribute `name` in the nearest class of the add-on's method
/// resolution order that binds it, with its value when that is a string literal. The `bpy.types`
/// classes only declare it, so they end the search.
fn class_attribute<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<(Definition<'db>, Option<String>)> {
    for base in mro_classes(db, class) {
        if is_bpy_types_class(db, base) {
            return None;
        }
        if let Some(binding) = own_binding(db, base, name) {
            return Some(binding);
        }
    }
    None
}

/// The last binding of `name` in the body of `class`, with its value when that is a string
/// literal.
fn own_binding<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    name: &str,
) -> Option<(Definition<'db>, Option<String>)> {
    let scope = class.body_scope(db);
    let symbol = place_table(db, scope).symbol_id(name)?;
    let definition = use_def_map(db, scope)
        .end_of_scope_symbol_bindings(symbol)
        .filter_map(|binding| binding.binding.definition())
        .last()?;
    let value = binding_type(db, definition)
        .as_literal_value()
        .and_then(LiteralValueType::as_string)
        .map(|string| string.value(db).to_string())
        .or_else(|| folded_idname(db, definition).clone());
    Some((definition, value))
}

/// The value of `bl_idname = Other.bl_idname + "_copy"`, which ty widens to `str`: the class
/// attribute is declared `bl_idname: str` in `bpy.types`. The other class's idname is read as
/// Blender does, and the string literals after it are appended.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size=ruff_memory_usage::heap_size
)]
fn folded_idname<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Option<String> {
    let DefinitionKind::Assignment(assignment) = definition.kind(db) else {
        return None;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let mut value = assignment.value(&module);
    let mut suffix = String::new();
    while let Expr::BinOp(ast::ExprBinOp {
        left,
        op: ast::Operator::Add,
        right,
        ..
    }) = value
    {
        let Expr::StringLiteral(string) = &**right else {
            return None;
        };
        suffix.insert_str(0, string.value.to_str());
        value = left;
    }
    let Expr::Attribute(attribute) = value else {
        return None;
    };
    if suffix.is_empty() || attribute.attr.as_str() != "bl_idname" {
        return None;
    }
    let owner =
        definition_expression_type_in_scope(db, definition, &attribute.value, definition.scope(db));
    let Type::ClassLiteral(owner) = owner else {
        return None;
    };
    let (_, idname) = class_attribute(db, owner.as_static()?, "bl_idname")?;
    Some(idname? + &suffix)
}

/// The operator idname `class` registers under, when it is one Blender accepts: its `bl_idname`
/// as Blender reads it from the class and the classes it derives from.
pub(super) fn operator_idname(
    db: &dyn Db,
    class: StaticClassLiteral<'_>,
) -> Option<OperatorIdname> {
    let (_, idname) = class_attribute(db, class, "bl_idname")?;
    OperatorIdname::from_bl_idname(&idname?)
}
