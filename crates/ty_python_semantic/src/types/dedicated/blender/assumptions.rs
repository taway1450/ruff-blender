//! Assumptions: facts Blender keeps in ordinary use but does not guarantee. The code is accepted and
//! typed as if the fact held, and reported under a rule of the category `blender-assumptions`
//! ([`super::categories`]):
//!
//! - `blender-assumed-context-member`: `scene`, `view_layer`, `tool_settings` and `collection`
//!   outside a Blender callback, reported by [`super::context_members`];
//! - `blender-assumed-list-item`: `UIList.draw_item`'s `item` is not `None`
//!   ([`ty_blender::Callback::assumed_item_position`], typed by [`super::overrides`], by its
//!   source where the add-on shows the list ([`super::list_sources`])), reported at each read here;
//! - `blender-assumed-registration`: a class or Registered property that `register()` registers
//!   only on a branch counts as registered, reported at its uses (`bpy.ops.x.y`, a Registered
//!   property read, `.preferences`);
//! - `blender-assumed-complete-registration`: the `register()` walk gave up, so every class and
//!   property of the add-on counts as registered; reported once, at the value it could not follow;
//! - `blender-assumed-preferences`: `addons[...].preferences` keeps the add-on's class while
//!   `register()` runs before the class is registered, or `unregister()` after it is unregistered.
//! - `blender-assumed-internal-operator`, `blender-assumed-editor-member`,
//!   `blender-assumed-preset-context` and `blender-assumed-added-object`: a context member that an
//!   Internal operator, an editor's UI callback, a preset or a call that adds an object takes as
//!   set, reported by [`super::context_assumptions`].
//! - `blender-assumed-default-addon`: a property that an add-on Blender enables by default
//!   registers (`scene.cycles`) is read as registered, though a user can disable the add-on; found
//!   by [`super::external`], reported at each read here.
//! - `blender-assumed-subclass-member`: a member a refined base lacks but some of its tagged
//!   subclasses declare, read as the union of their declarations, reported by
//!   [`super::subclass_members`].

use ruff_db::parsed::ParsedModuleRef;
use ruff_python_ast::name::Name;
use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt as walk_expr_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::{ProgramFile, semantic_index};

use crate::Db;
use crate::types::StaticClassLiteral;
use crate::types::context::InferContext;

use super::index::{AddonInProgram, addon_in_program, addon_index};
use super::lints::{
    BLENDER_ASSUMED_COMPLETE_REGISTRATION, BLENDER_ASSUMED_DEFAULT_ADDON,
    BLENDER_ASSUMED_LIST_ITEM, BLENDER_ASSUMED_PREFERENCES, BLENDER_ASSUMED_REGISTRATION,
};
use super::member::without_none;
use super::overrides::{assumed_item_type, override_types};
use super::poll_checks::name_tests;
use super::preferences::addon_preferences;
use super::walk::{Event, Site, register_walk, unregister_walk};

/// Reports the Assumptions that belong to `file` as a whole: the reads of `UIList.draw_item`'s
/// `item`, and where the `register()` walk of the file's add-on gave up.
pub(super) fn check_assumptions(
    context: &InferContext,
    file: ProgramFile,
    module: &ParsedModuleRef,
) {
    let db = context.db();
    if let Some(key) = addon_in_program(db, file)
        && let Some(site) = register_walk(db, key).incomplete
        && site.file == context.file()
        && !site.range.is_empty()
        && let Some(builder) =
            context.report_lint(&BLENDER_ASSUMED_COMPLETE_REGISTRATION, site.range)
    {
        builder.into_diagnostic(
            "`register()` registers a value the checker cannot follow here, so every class and \
             property of the add-on counts as registered, and classes `register()` misses are not \
             reported",
        );
    }
    let mut finder = DrawItems::default();
    finder.visit_body(module.suite());
    for function in finder.functions {
        check_list_item(context, file, function);
    }
}

/// The methods named `draw_item` in a class body, at any depth.
#[derive(Default)]
struct DrawItems<'a> {
    functions: Vec<&'a ast::StmtFunctionDef>,
}

impl<'a> StatementVisitor<'a> for DrawItems<'a> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if let Stmt::ClassDef(class) = statement {
            for statement in &class.body {
                if let Stmt::FunctionDef(function) = statement
                    && function.name.id == "draw_item"
                {
                    self.functions.push(function);
                }
            }
        }
        walk_stmt(self, statement);
    }
}

/// Reports the reads of the `item` parameter of `function`, a method named `draw_item`, when it
/// overrides `UIList.draw_item`, leaves `item` unannotated and the parent's `item` may be `None`.
/// A read only tested for truth or `None`, or one where the method's own test narrows it, is not
/// reported; nor is any read when the method rebinds the name.
fn check_list_item(context: &InferContext, file: ProgramFile, function: &ast::StmtFunctionDef) {
    let db = context.db();
    let index = semantic_index(db, file);
    let Some(definition) = index.try_definition(function) else {
        return;
    };
    let Some(types) = override_types(db, definition) else {
        return;
    };
    let Some(position) = types.assumed_item_position else {
        return;
    };
    let Some(parent) = types.positional(position) else {
        return;
    };
    let narrowed = without_none(db, parent);
    if narrowed == parent {
        return;
    }
    let narrowed = assumed_item_type(db, definition, narrowed);
    let parameters = &function.parameters;
    let Some(parameter) = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .nth(position)
    else {
        return;
    };
    if parameter.parameter.annotation.is_some() {
        return;
    }
    let name = &parameter.parameter.name.id;
    let mut reads = NameReads {
        name,
        reads: Vec::new(),
        rebound: false,
    };
    reads.visit_body(&function.body);
    if reads.rebound {
        return;
    }
    let (tested, regions) = name_tests(&function.body, name);
    let env = context.program_environment();
    for read in reads.reads {
        if tested.contains(&read) || regions.iter().any(|region| region.contains_range(read)) {
            continue;
        }
        if let Some(builder) = context.report_lint(&BLENDER_ASSUMED_LIST_ITEM, read) {
            builder.into_diagnostic(format_args!(
                "`{name}` is taken as `{}`: Blender passes `None` to `UIList.draw_item` only for an \
                 empty slot of an array of ID pointers",
                narrowed.display(db, env),
            ));
        }
    }
}

/// The reads of a name in a function body, also in lambdas and comprehensions; whether the body
/// binds the name again (an assignment, a loop target, a nested function's parameter, ...).
struct NameReads<'a> {
    name: &'a Name,
    reads: Vec<TextRange>,
    rebound: bool,
}

impl<'a> Visitor<'a> for NameReads<'_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::FunctionDef(function) => {
                if function.name.id == *self.name
                    || function
                        .parameters
                        .iter()
                        .any(|parameter| parameter.name().id == *self.name)
                {
                    self.rebound = true;
                }
                walk_expr_stmt(self, statement);
            }
            Stmt::ClassDef(class) if class.name.id == *self.name => self.rebound = true,
            Stmt::Global(ast::StmtGlobal { names, .. })
            | Stmt::Nonlocal(ast::StmtNonlocal { names, .. })
                if names.iter().any(|name| name.id == *self.name) =>
            {
                self.rebound = true;
            }
            Stmt::Import(ast::StmtImport { names, .. })
            | Stmt::ImportFrom(ast::StmtImportFrom { names, .. })
                if names
                    .iter()
                    .any(|alias| alias.asname.as_ref().unwrap_or(&alias.name).id == *self.name) =>
            {
                self.rebound = true;
            }
            _ => walk_expr_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Name(name) if name.id == *self.name => {
                if name.ctx.is_load() {
                    self.reads.push(name.range);
                } else {
                    self.rebound = true;
                }
            }
            Expr::Lambda(lambda)
                if lambda.parameters.as_ref().is_some_and(|parameters| {
                    parameters
                        .iter()
                        .any(|parameter| parameter.name().id == *self.name)
                }) =>
            {
                self.rebound = true;
            }
            _ => walk_expr(self, expression),
        }
    }
}

/// Whether `class` is one that `register()` of `key` registers only on a branch.
pub(super) fn is_conditional_class<'db>(
    db: &'db dyn Db,
    key: AddonInProgram<'db>,
    class: StaticClassLiteral<'db>,
) -> bool {
    addon_index(db, key).conditional_classes.contains(&class)
}

/// Reports `site`, the use `what` of `class`, which `register()` registers only on a branch.
pub(super) fn report_conditional_class(
    context: &InferContext,
    site: impl Ranged,
    what: &str,
    class: StaticClassLiteral,
) {
    if let Some(builder) = context.report_lint(&BLENDER_ASSUMED_REGISTRATION, site) {
        builder.into_diagnostic(format_args!(
            "{what} is taken as registered, but `register()` registers `{}` only on a branch (a \
             condition, an `except` handler, a timer or a list extended at run time)",
            class.name(context.db()),
        ));
    }
}

/// Reports `site`, a read of the Registered property `owner.name`, when `register()` assigns it
/// only on a branch.
pub(super) fn report_conditional_property<'db>(
    context: &InferContext<'db, '_>,
    site: impl Ranged,
    owner: StaticClassLiteral<'db>,
    name: &Name,
) {
    let db = context.db();
    let Some(key) = addon_in_program(db, context.program_file()) else {
        return;
    };
    if !addon_index(db, key).conditional_properties.iter().any(
        |(conditional_owner, conditional_name)| {
            *conditional_owner == owner && conditional_name == name
        },
    ) {
        return;
    }
    if let Some(builder) = context.report_lint(&BLENDER_ASSUMED_REGISTRATION, site) {
        builder.into_diagnostic(format_args!(
            "`{}.{name}` is taken as registered, but `register()` assigns it only on a branch (a \
             condition, an `except` handler, a timer or a list extended at run time)",
            owner.name(db),
        ));
    }
}

/// Reports a read of a property that the add-on `addon`, which Blender enables by default,
/// registers.
pub(super) fn report_default_addon(
    context: &InferContext,
    attribute: &ast::ExprAttribute,
    addon: &Name,
) {
    if let Some(builder) = context.report_lint(&BLENDER_ASSUMED_DEFAULT_ADDON, attribute) {
        builder.into_diagnostic(format_args!(
            "`{}` is taken as registered by `{addon}`, an add-on Blender enables by default but a \
             user can disable",
            attribute.attr.id,
        ));
    }
}

/// Reports the read `attribute` of an add-on's preferences (`addons[...].preferences`), which the
/// checker typed as the add-on's preferences class: under `blender-assumed-preferences` when it
/// runs while `register()` has not registered the class yet, or `unregister()` has unregistered
/// it, and else under `blender-assumed-registration` when the class is registered only on a
/// branch.
pub(super) fn report_preferences_read(context: &InferContext, attribute: &ast::ExprAttribute) {
    let db = context.db();
    let Some(key) = addon_in_program(db, context.program_file()) else {
        return;
    };
    let site = Site {
        file: context.file(),
        range: attribute.range(),
    };
    if early_preferences_reads(db, key).contains(&site) {
        if let Some(builder) = context.report_lint(&BLENDER_ASSUMED_PREFERENCES, attribute) {
            builder.into_diagnostic(
                "The add-on's preferences are taken as its preferences class, but this read runs \
                 while the class is not registered (in `register()` before `register_class`, or \
                 in `unregister()` after `unregister_class`), where `preferences` is `None`",
            );
        }
        return;
    }
    if let Some(class) = addon_preferences(db, key)
        .classes
        .iter()
        .find(|class| is_conditional_class(db, key, **class))
    {
        report_conditional_class(context, attribute, "The add-on's preferences class", *class);
    }
}

/// The reads of an add-on's preferences that run while its preferences class is not registered:
/// in `register()` (the root module's body included) before the first `register_class` of a
/// preferences class, and in `unregister()` after an `unregister_class` of one. Reads in functions
/// Blender calls later (a timer, a handler) run after `register()` returned, and are left out.
#[salsa::tracked(
    returns(deref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
fn early_preferences_reads<'db>(db: &'db dyn Db, key: AddonInProgram<'db>) -> Box<[Site]> {
    let classes = &addon_preferences(db, key).classes;
    if classes.is_empty() {
        return Box::default();
    }
    let mut reads: Vec<Site> = Vec::new();
    let mut registered = false;
    for event in &register_walk(db, key).events {
        match event {
            Event::RegisterClass(event) if classes.contains(&event.class) => registered = true,
            Event::PreferencesRead {
                site,
                deferred: false,
            } if !registered && !reads.contains(site) => reads.push(*site),
            _ => {}
        }
    }
    let mut unregistered = false;
    for event in &unregister_walk(db, key).events {
        match event {
            Event::UnregisterClass(event) if classes.contains(&event.class) => unregistered = true,
            Event::PreferencesRead {
                site,
                deferred: false,
            } if unregistered && !reads.contains(site) => reads.push(*site),
            _ => {}
        }
    }
    reads.into_boxed_slice()
}
