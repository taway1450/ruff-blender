//! The `register()` walk: what an add-on's `register()` and `unregister()` do to Blender, found by a
//! small abstract interpreter rather than by inferring the add-on's code.
//!
//! 1. [`registration_ir`] turns each file into *steps*, once per file and without keeping its syntax
//!    tree: the module body and every function body become lists of calls, assignments, property
//!    registrations, imports, loops and branches. Names are resolved through the definitions of
//!    their names (the shared follower). Parameters and loop variables stay symbolic, and a
//!    module-level name that is assigned (`classes = (A, B)`) stays a reference to its bindings.
//! 2. [`register_walk`] and [`unregister_walk`] interpret the steps, starting with the root module's
//!    body and then the entry function. They follow calls into first-party functions in any file
//!    (shared libraries included), run each module body once at its first import, and evaluate
//!    sequences from the syntax: displays, `*x`, `+`, `reversed()`, comprehensions, returns and the
//!    `append` / `extend` / `insert` calls on a list in its file. Blender's two factories are
//!    modelled on Blender's own code.
//!
//! The result is the ordered list of what happened ([`Event`]), each event with the branches it
//! depends on ([`BranchPath`]) and whether its order is known (`exact`). A value the walk cannot know
//! (`getattr(...)`, `importlib.import_module(...)`, an unbound parameter) is *opaque*; the walk gives
//! up ([`Walk::incomplete`]) only when such a value is registered, called or assigned a property to.
//! Values from Blender or the standard library (`bpy.context`, `os.environ`) are *foreign* and never
//! make it give up.

use std::rc::Rc;

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{ImportingFile, ModuleName, file_to_module};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::{NodeWithScopeRef, ScopeId, ScopeKind};
use ty_python_core::{
    ProgramFile, SemanticIndex, global_scope, place_table, semantic_index, use_def_map,
};

use crate::Db;
use crate::types::definition_resolution::{ResolvedDefinition, find_symbol_in_scope};
use crate::types::{StaticClassLiteral, Type, binding_type};

use super::addon::is_first_party;
use super::evaluate::{Evaluator, PropertyValue};
use super::follow::{
    Resolved, imported_definitions, module_by_name, resolve_expression, resolve_name_in,
};
use super::index::{AddonInProgram, RegisteredProperty, accepts_registered_properties};
use super::symbols::{bpy_types_bases, is_bpy_types_class};

/// A place in a file, where a diagnostic can be reported.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct Site {
    pub(super) file: File,
    pub(super) range: TextRange,
}

/// A branching statement, as the walk executed it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) enum BranchId {
    /// A statement of a module body, which runs once: the statement itself identifies it.
    Static { file: File, offset: TextSize },
    /// A statement of a function body, numbered each time the walk executes it: the same `if` in
    /// two calls or two loop iterations is two branches, where both arms may run.
    Dynamic(u32),
}

/// The branches an event depends on, outermost first: each with the arm the walk took.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct BranchPath(Box<[(BranchId, u8)]>);

impl BranchPath {
    /// Whether two events cannot both happen: they took different arms of one branch.
    pub(super) fn is_exclusive_with(&self, other: &BranchPath) -> bool {
        self.0.iter().any(|(id, arm)| {
            other
                .0
                .iter()
                .any(|(other_id, other_arm)| id == other_id && arm != other_arm)
        })
    }
}

/// A class registered or unregistered.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct ClassEvent<'db> {
    pub(super) class: StaticClassLiteral<'db>,
    /// Where the class was named: the argument of `register_class(C)`, or the element of the
    /// sequence a loop or a factory registered.
    pub(super) named_at: Site,
    pub(super) path: BranchPath,
    /// Whether the event's order and count are known. Events of functions Blender calls later
    /// (timers, handlers), of unordered sequences and of lists mutated in functions are not.
    pub(super) exact: bool,
}

/// What a handler or menu function is added to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) enum HookTarget<'db> {
    /// A class: a menu, panel or header, of the add-on or of Blender.
    Class(StaticClassLiteral<'db>),
    /// A path the stubs do not name a class for: `bpy.app.handlers.load_post`,
    /// `bpy.types.VIEW3D_MT_object`.
    Path(Box<str>),
}

/// What `register()` or `unregister()` did, in order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) enum Event<'db> {
    RegisterClass(ClassEvent<'db>),
    UnregisterClass(ClassEvent<'db>),
    SetProperty {
        property: RegisteredProperty<'db>,
        /// The assignment target (`bpy.types.Scene.x`), or the `setattr` call.
        site: Site,
        path: BranchPath,
        exact: bool,
    },
    DelProperty {
        owner: StaticClassLiteral<'db>,
        name: Name,
    },
    Hook {
        target: HookTarget<'db>,
        function: Definition<'db>,
        /// `append` / `prepend`, rather than `remove`.
        added: bool,
        /// The `append(f)` call.
        site: Site,
    },
}

/// The result of a walk.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct Walk<'db> {
    pub(super) events: Box<[Event<'db>]>,
    /// Every first-party function the walk entered.
    pub(super) entered: Box<[Definition<'db>]>,
    /// Where the walk gave up, when it did: the events are then only part of what happens.
    pub(super) incomplete: Option<Site>,
}

impl Walk<'_> {
    pub(super) fn is_complete(&self) -> bool {
        self.incomplete.is_none()
    }
}

/// What `register()` does. A cycle (inferring a class the walk names may read the Add-on index,
/// which reads this walk) starts from an empty walk.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| Walk::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn register_walk<'db>(db: &'db dyn Db, key: AddonInProgram<'db>) -> Walk<'db> {
    walk(db, key, Entry::Register)
}

/// What `unregister()` does.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| Walk::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn unregister_walk<'db>(db: &'db dyn Db, key: AddonInProgram<'db>) -> Walk<'db> {
    walk(db, key, Entry::Unregister)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Entry {
    Register,
    Unregister,
}

impl Entry {
    fn name(self) -> &'static str {
        match self {
            Entry::Register => "register",
            Entry::Unregister => "unregister",
        }
    }
}

fn walk<'db>(db: &'db dyn Db, key: AddonInProgram<'db>, entry: Entry) -> Walk<'db> {
    let program = key.program(db);
    let root = ruff_db::files::system_path_to_file(db, key.addon(db).root_module())
        .ok()
        .map(|file| ProgramFile::new(db, file, program));
    let Some(root) = root else {
        return Walk::default();
    };
    let mut walker = Walker::new(db);
    walker.run(root, entry);
    walker.finish()
}

// The per-file steps.

/// The steps of one file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct FileIr<'db> {
    module: Body<'db>,
    functions: Box<[FunctionIr<'db>]>,
    globals: Box<[GlobalName<'db>]>,
}

impl<'db> FileIr<'db> {
    fn function(&self, definition: Definition<'db>) -> Option<&FunctionIr<'db>> {
        self.functions
            .iter()
            .find(|function| function.definition == definition)
    }

    fn global(&self, name: &str) -> Option<&GlobalName<'db>> {
        self.globals.iter().find(|global| global.name == name)
    }

    /// The functions of the file whose own body registers a class: it calls `register_class`, or
    /// the register half of a `register_classes_factory`.
    pub(super) fn registering_functions(
        &self,
        db: &'db dyn Db,
    ) -> impl Iterator<Item = &FunctionIr<'db>> {
        self.functions.iter().filter(move |function| {
            function.registers
                || function
                    .global_callees
                    .iter()
                    .any(|(file, name)| is_register_half(db, *file, name))
        })
    }
}

/// The branches of a module-level statement: each `if` or `try` it is in, by its offset, with the arm.
type StaticPath = Box<[(TextSize, u8)]>;

/// A function or method, at any depth.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(super) struct FunctionIr<'db> {
    pub(super) definition: Definition<'db>,
    /// The function's name in its `def`.
    pub(super) name_range: TextRange,
    parameters: Box<[Parameter<'db>]>,
    body: Body<'db>,
    /// Whether its own body calls `register_class`.
    registers: bool,
    /// The classes its own `register_class` calls name.
    pub(super) named_classes: Box<[StaticClassLiteral<'db>]>,
    /// The module-level names it calls (`_register()`), which may be a factory's register half.
    global_callees: Box<[(ProgramFile<'db>, Name)]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct Parameter<'db> {
    name: Name,
    kind: ParameterKind,
    default: Option<Val<'db>>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum ParameterKind {
    Positional,
    PositionalOrKeyword,
    KeywordOnly,
    /// `*args` and `**kwargs`: their values are never known.
    Variadic,
}

/// A module-level name that assignments bind, with what makes up its value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct GlobalName<'db> {
    name: Name,
    entries: Box<[GlobalEntry<'db>]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct GlobalEntry<'db> {
    kind: EntryKind<'db>,
    /// The module-level branches of the statement.
    path: Box<[(TextSize, u8)]>,
    /// False for a mutation in a function body, whose place in the order is not known.
    exact: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum EntryKind<'db> {
    /// `name = value`; `index` is the name's position in an unpacking target
    /// (`register, unregister = ...` binds `register` at `[0]`).
    Bind { value: Val<'db>, index: Box<[u32]> },
    /// `name += value`, `name.extend(value)`.
    Extend(Val<'db>),
    /// `name.append(value)`, `name.insert(i, value)`.
    Append(Val<'db>),
    /// A binding the walk does not read (a loop target, a `with` target).
    Opaque(TextRange),
}

type Body<'db> = Box<[Step<'db>]>;

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum Step<'db> {
    /// Evaluate an expression for its calls.
    Eval(Val<'db>),
    /// Bind a function's local names.
    Assign(Target, Val<'db>),
    /// `owner.name = <property>` or `setattr(owner, name, <property>)`.
    SetProperty {
        owner: Val<'db>,
        owner_range: TextRange,
        name: Val<'db>,
        value: PropertyValue<'db>,
        site: TextRange,
    },
    /// `del owner.name`.
    DelProperty {
        owner: Val<'db>,
        name: Name,
    },
    /// Run a module's body, at its first import.
    Import(ProgramFile<'db>),
    For {
        target: Target,
        iterable: Val<'db>,
        body: Body<'db>,
    },
    /// `if` / `elif` / `else`, `match`, or the handlers and `else` of a `try`: one arm runs.
    Branch {
        offset: TextSize,
        arms: Box<[Body<'db>]>,
    },
    Return(Val<'db>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum Target {
    Name(Name),
    Tuple(Box<[Target]>),
    /// An attribute, a subscript or a starred target.
    Other,
}

/// An expression, with its names resolved as far as the syntax allows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum Val<'db> {
    Module(ProgramFile<'db>),
    Class(StaticClassLiteral<'db>, TextRange),
    /// A first-party function.
    Function(Definition<'db>),
    Blender(BlenderFunction),
    /// A function that discovers code at runtime (`importlib.import_module`, `getattr`).
    Discovery,
    /// A name bound by the walk: a parameter, loop variable or local assignment.
    Local(Name, TextRange),
    /// A module-level name of a file that assignments bind.
    Global(ProgramFile<'db>, Name, TextRange),
    Str(Name),
    /// A tuple, list or set display.
    Display {
        elements: Box<[Val<'db>]>,
        ordered: bool,
    },
    Starred(Box<Val<'db>>),
    Add(Box<Val<'db>>, Box<Val<'db>>),
    Call {
        callee: Box<Val<'db>>,
        arguments: Box<[Val<'db>]>,
        keywords: Box<[(Name, Val<'db>)]>,
        range: TextRange,
    },
    Attribute(Box<Val<'db>>, Name, TextRange),
    Comprehension {
        element: Box<Val<'db>>,
        target: Target,
        iterable: Box<Val<'db>>,
        filtered: bool,
    },
    /// `a if c else b`.
    Either(TextSize, Box<[Val<'db>]>),
    /// Something outside first-party code, with its dotted path when it is known.
    Foreign(Option<Box<str>>),
    Opaque(TextRange),
}

/// The functions of Blender and Python the walk understands.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum BlenderFunction {
    RegisterClass,
    UnregisterClass,
    ClassesFactory,
    SubmoduleFactory,
    TimerRegister,
    Setattr,
    Delattr,
    Reversed,
    /// `list(x)` and `tuple(x)`.
    Copy,
    /// `sorted(x)`, `set(x)`: the elements, in an order the walk does not know.
    Unordered,
}

/// The steps of `file`. The syntax tree is dropped afterwards; only the steps are kept, so an edit
/// that does not change them (inside a method, or in a check module) does not re-run the walks.
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| FileIr::default(),
    heap_size=ruff_memory_usage::heap_size
)]
pub(super) fn registration_ir<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> FileIr<'db> {
    let module = parsed_module(db, file.python_file(db)).load_then_clear_on_drop(db);
    let index = semantic_index(db, file);
    let mut builder = IrBuilder {
        db,
        file,
        index,
        module: &module,
        functions: Vec::new(),
        mutations: Vec::new(),
        statements: Vec::new(),
        assigned: Vec::new(),
        path: Vec::new(),
        current: None,
    };
    let scope = global_scope(db, file);
    let body = builder.body(module.suite(), scope, true);
    let globals = builder.globals();
    FileIr {
        module: body,
        functions: builder.functions.into_boxed_slice(),
        globals,
    }
}

/// What the builder records about the function whose body it is in.
#[derive(Default)]
struct FunctionFacts<'db> {
    registers: bool,
    named_classes: Vec<StaticClassLiteral<'db>>,
    global_callees: Vec<(ProgramFile<'db>, Name)>,
}

struct IrBuilder<'db, 'm> {
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    index: &'db SemanticIndex<'db>,
    module: &'m ParsedModuleRef,
    functions: Vec<FunctionIr<'db>>,
    /// `name.append(x)` and the like, wherever `name` is a module-level name of this file.
    mutations: Vec<(Name, GlobalEntry<'db>)>,
    /// The module-level statements, with their branches.
    statements: Vec<(TextRange, StaticPath)>,
    /// The module-level names that assignments bind.
    assigned: Vec<Name>,
    /// The module-level branches of the statement being built.
    path: Vec<(TextSize, u8)>,
    current: Option<FunctionFacts<'db>>,
}

impl<'db> IrBuilder<'db, '_> {
    fn body(&mut self, statements: &[Stmt], scope: ScopeId<'db>, module_level: bool) -> Body<'db> {
        let mut steps = Vec::new();
        for statement in statements {
            self.statement(statement, scope, module_level, &mut steps);
        }
        steps.into_boxed_slice()
    }

    /// A body nested in a branch arm: at module level, its statements get the arm's path.
    fn arm(
        &mut self,
        statements: &[Stmt],
        scope: ScopeId<'db>,
        module_level: bool,
        offset: TextSize,
        arm: u8,
    ) -> Body<'db> {
        if module_level {
            self.path.push((offset, arm));
        }
        let body = self.body(statements, scope, module_level);
        if module_level {
            self.path.pop();
        }
        body
    }

    #[expect(clippy::too_many_lines)]
    fn statement(
        &mut self,
        statement: &Stmt,
        scope: ScopeId<'db>,
        module_level: bool,
        steps: &mut Vec<Step<'db>>,
    ) {
        if module_level {
            self.statements
                .push((statement.range(), self.path.clone().into_boxed_slice()));
        }
        match statement {
            Stmt::FunctionDef(function) => self.function(function),
            Stmt::ClassDef(class) => self.class(class),
            Stmt::Expr(expression) => {
                self.expression_statement(&expression.value, scope, module_level, steps);
            }
            Stmt::Assign(assign) => {
                self.assignment(&assign.targets, &assign.value, scope, module_level, steps);
            }
            Stmt::AnnAssign(assign) => {
                if let Some(value) = &assign.value {
                    self.assignment(
                        std::slice::from_ref(&*assign.target),
                        value,
                        scope,
                        module_level,
                        steps,
                    );
                }
            }
            Stmt::AugAssign(assign) => {
                let value = self.expression(&assign.value, scope);
                match &*assign.target {
                    Expr::Name(name) if module_level => {
                        self.assigned.push(name.id.clone());
                        steps.push(Step::Eval(value));
                    }
                    Expr::Name(name)
                        if assign.op == ast::Operator::Add && self.is_local(scope, &name.id) =>
                    {
                        steps.push(Step::Assign(
                            Target::Name(name.id.clone()),
                            Val::Add(
                                Box::new(Val::Local(name.id.clone(), name.range)),
                                Box::new(value),
                            ),
                        ));
                    }
                    _ => steps.push(Step::Eval(value)),
                }
            }
            Stmt::Delete(delete) => {
                for target in &delete.targets {
                    if let Expr::Attribute(attribute) = target {
                        let owner = self.expression(&attribute.value, scope);
                        steps.push(Step::DelProperty {
                            owner,
                            name: attribute.attr.id.clone(),
                        });
                    }
                }
            }
            Stmt::If(if_statement) => {
                steps.push(Step::Eval(self.expression(&if_statement.test, scope)));
                let offset = if_statement.start();
                let mut arms = vec![self.arm(&if_statement.body, scope, module_level, offset, 0)];
                let mut has_else = false;
                for (clause, arm) in if_statement.elif_else_clauses.iter().zip(1u8..) {
                    has_else |= clause.test.is_none();
                    arms.push(self.arm(&clause.body, scope, module_level, offset, arm));
                }
                if !has_else {
                    arms.push(Box::default());
                }
                steps.push(Step::Branch {
                    offset,
                    arms: arms.into_boxed_slice(),
                });
            }
            Stmt::For(for_statement) => {
                let iterable = self.expression(&for_statement.iter, scope);
                let target = assignment_target(&for_statement.target);
                let body = self.body(&for_statement.body, scope, module_level);
                steps.push(Step::For {
                    target,
                    iterable,
                    body,
                });
                steps.extend(self.body(&for_statement.orelse, scope, module_level));
            }
            Stmt::While(while_statement) => {
                steps.push(Step::Eval(self.expression(&while_statement.test, scope)));
                steps.extend(self.body(&while_statement.body, scope, module_level));
                steps.extend(self.body(&while_statement.orelse, scope, module_level));
            }
            Stmt::Try(try_statement) => {
                // The body may register and then raise, so it runs whatever handler follows.
                steps.extend(self.body(&try_statement.body, scope, module_level));
                let offset = try_statement.start();
                let mut arms = Vec::new();
                for (handler, arm) in try_statement.handlers.iter().zip(0u8..) {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    arms.push(self.arm(&handler.body, scope, module_level, offset, arm));
                }
                let arm = u8::try_from(arms.len()).unwrap_or(u8::MAX);
                arms.push(self.arm(&try_statement.orelse, scope, module_level, offset, arm));
                steps.push(Step::Branch {
                    offset,
                    arms: arms.into_boxed_slice(),
                });
                steps.extend(self.body(&try_statement.finalbody, scope, module_level));
            }
            Stmt::With(with_statement) => {
                for item in &with_statement.items {
                    steps.push(Step::Eval(self.expression(&item.context_expr, scope)));
                }
                steps.extend(self.body(&with_statement.body, scope, module_level));
            }
            Stmt::Match(match_statement) => {
                steps.push(Step::Eval(self.expression(&match_statement.subject, scope)));
                let offset = match_statement.start();
                let mut arms = Vec::new();
                for (case, arm) in match_statement.cases.iter().zip(0u8..) {
                    arms.push(self.arm(&case.body, scope, module_level, offset, arm));
                }
                steps.push(Step::Branch {
                    offset,
                    arms: arms.into_boxed_slice(),
                });
            }
            Stmt::Return(return_statement) => {
                let value = return_statement
                    .value
                    .as_deref()
                    .map_or(Val::Foreign(None), |value| self.expression(value, scope));
                steps.push(Step::Return(value));
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let mut name = String::new();
                    for part in alias.name.split('.') {
                        if !name.is_empty() {
                            name.push('.');
                        }
                        name.push_str(part);
                        if let Some(module) = self.first_party_module(&name) {
                            steps.push(Step::Import(module));
                        }
                    }
                    if !module_level {
                        let (local, bound) = match &alias.asname {
                            Some(asname) => (asname.id.clone(), alias.name.as_str()),
                            None => {
                                let first = alias.name.split('.').next().unwrap_or_default();
                                (Name::new(first), first)
                            }
                        };
                        let value = module_by_name(self.db, self.file, bound)
                            .map_or(Val::Foreign(None), Val::Module);
                        steps.push(Step::Assign(Target::Name(local), value));
                    }
                }
            }
            Stmt::ImportFrom(import) => {
                let importing = ImportingFile::ResolverFile(self.file.resolver_file(self.db));
                if let Ok(base) = ModuleName::from_import_statement(self.db, importing, import)
                    && let Some(module) = self.first_party_module(base.as_str())
                {
                    steps.push(Step::Import(module));
                }
                for alias in &import.names {
                    if alias.name.as_str() == "*" {
                        continue;
                    }
                    let local = alias.asname.as_ref().unwrap_or(&alias.name);
                    let value = match resolve_name_in(self.db, scope, local.as_str()) {
                        Some(Resolved::Module(module)) => {
                            if is_first_party(self.db, module) {
                                steps.push(Step::Import(module));
                            }
                            Val::Module(module)
                        }
                        _ => self.name(scope, local.as_str(), local.range),
                    };
                    if !module_level {
                        steps.push(Step::Assign(Target::Name(local.id.clone()), value));
                    }
                }
            }
            _ => {}
        }
    }

    /// The module `name`, when it is a first-party module.
    fn first_party_module(&self, name: &str) -> Option<ProgramFile<'db>> {
        module_by_name(self.db, self.file, name).filter(|module| is_first_party(self.db, *module))
    }

    fn function(&mut self, function: &ast::StmtFunctionDef) {
        let db = self.db;
        let Some(definition) = self.index.try_definition(function) else {
            return;
        };
        let parent_scope = definition.scope(db);
        let scope = self
            .index
            .node_scope(NodeWithScopeRef::Function(function))
            .to_scope_id(db, self.file);
        let mut parameters = Vec::new();
        let ast::Parameters {
            posonlyargs,
            args,
            vararg,
            kwonlyargs,
            kwarg,
            ..
        } = &*function.parameters;
        for (list, kind) in [
            (posonlyargs, ParameterKind::Positional),
            (args, ParameterKind::PositionalOrKeyword),
            (kwonlyargs, ParameterKind::KeywordOnly),
        ] {
            for parameter in list {
                let default = parameter
                    .default
                    .as_deref()
                    .map(|default| self.expression(default, parent_scope));
                parameters.push(Parameter {
                    name: parameter.parameter.name.id.clone(),
                    kind,
                    default,
                });
            }
        }
        for parameter in [vararg, kwarg].into_iter().flatten() {
            parameters.push(Parameter {
                name: parameter.name.id.clone(),
                kind: ParameterKind::Variadic,
                default: None,
            });
        }
        let outer = self.current.replace(FunctionFacts::default());
        let body = self.body(&function.body, scope, false);
        let facts = std::mem::replace(&mut self.current, outer).unwrap_or_default();
        self.functions.push(FunctionIr {
            definition,
            name_range: function.name.range,
            parameters: parameters.into_boxed_slice(),
            body,
            registers: facts.registers,
            named_classes: facts.named_classes.into_boxed_slice(),
            global_callees: facts.global_callees.into_boxed_slice(),
        });
    }

    /// The methods and nested classes of a class body. The body's other statements are not steps:
    /// they define the class rather than register anything.
    fn class(&mut self, class: &ast::StmtClassDef) {
        for statement in &class.body {
            match statement {
                Stmt::FunctionDef(function) => self.function(function),
                Stmt::ClassDef(class) => self.class(class),
                _ => {}
            }
        }
    }

    fn expression_statement(
        &mut self,
        expression: &Expr,
        scope: ScopeId<'db>,
        module_level: bool,
        steps: &mut Vec<Step<'db>>,
    ) {
        if let Expr::Call(call) = expression
            && call.arguments.keywords.is_empty()
        {
            // `setattr(owner, "name", <property>)`.
            if let Expr::Name(function) = &*call.func
                && function.id.as_str() == "setattr"
                && let [owner, name, value] = &*call.arguments.args
                && let Some(value) = self.property_value(scope, value)
            {
                let owner_value = self.expression(owner, scope);
                let name = self.expression(name, scope);
                steps.push(Step::SetProperty {
                    owner: owner_value,
                    owner_range: owner.range(),
                    name,
                    value,
                    site: call.range(),
                });
                return;
            }
            // `classes.append(C)` and the like, on a module-level name of this file.
            if let Expr::Attribute(attribute) = &*call.func
                && let Expr::Name(list) = &*attribute.value
                && self.is_module_name(scope, &list.id)
            {
                let kind = match (attribute.attr.as_str(), &*call.arguments.args) {
                    ("append", [element]) | ("insert", [_, element]) => {
                        Some(EntryKind::Append(self.expression(element, scope)))
                    }
                    ("extend", [elements]) => {
                        Some(EntryKind::Extend(self.expression(elements, scope)))
                    }
                    _ => None,
                };
                if let Some(kind) = kind {
                    let path = if module_level {
                        self.path.clone().into_boxed_slice()
                    } else {
                        Box::default()
                    };
                    self.mutations.push((
                        list.id.clone(),
                        GlobalEntry {
                            kind,
                            path,
                            exact: module_level,
                        },
                    ));
                }
            }
        }
        let value = self.expression(expression, scope);
        steps.push(Step::Eval(value));
    }

    fn assignment(
        &mut self,
        targets: &[Expr],
        value: &Expr,
        scope: ScopeId<'db>,
        module_level: bool,
        steps: &mut Vec<Step<'db>>,
    ) {
        let property = if targets.iter().any(Expr::is_attribute_expr) {
            self.property_value(scope, value)
        } else {
            None
        };
        let evaluated = self.expression(value, scope);
        if module_level {
            for target in targets {
                collect_names(target, &mut self.assigned);
            }
            steps.push(Step::Eval(evaluated.clone()));
        }
        for target in targets {
            match target {
                Expr::Attribute(attribute) => {
                    if let Some(property) = &property {
                        let owner = self.expression(&attribute.value, scope);
                        steps.push(Step::SetProperty {
                            owner,
                            owner_range: attribute.value.range(),
                            name: Val::Str(attribute.attr.id.clone()),
                            value: property.clone(),
                            site: attribute.range,
                        });
                    }
                }
                _ if module_level => {}
                _ => steps.push(Step::Assign(assignment_target(target), evaluated.clone())),
            }
        }
    }

    /// The property that `value` declares, read as the Add-on index reads a registration.
    fn property_value(&self, scope: ScopeId<'db>, value: &Expr) -> Option<PropertyValue<'db>> {
        if !matches!(
            value,
            Expr::Call(_) | Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_)
        ) {
            return None;
        }
        let db = self.db;
        let argument_type = |argument: &Expr| match resolve_expression(db, scope, argument) {
            Some(Resolved::Value(ty)) => ty,
            Some(Resolved::Function(definition)) => binding_type(db, definition),
            Some(Resolved::Module(_) | Resolved::Constant(_)) | None => Type::unknown(),
        };
        Evaluator::new(db, scope, &argument_type).evaluate(value)
    }

    fn expression(&mut self, expression: &Expr, scope: ScopeId<'db>) -> Val<'db> {
        match expression {
            Expr::Name(name) => self.name(scope, &name.id, name.range),
            Expr::Attribute(attribute) => Val::Attribute(
                Box::new(self.expression(&attribute.value, scope)),
                attribute.attr.id.clone(),
                attribute.range,
            ),
            Expr::Call(call) => self.call(call, scope),
            Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
                Val::Display {
                    elements: elts
                        .iter()
                        .map(|element| self.expression(element, scope))
                        .collect(),
                    ordered: true,
                }
            }
            Expr::Set(set) => Val::Display {
                elements: set
                    .elts
                    .iter()
                    .map(|element| self.expression(element, scope))
                    .collect(),
                ordered: false,
            },
            Expr::Starred(starred) => {
                Val::Starred(Box::new(self.expression(&starred.value, scope)))
            }
            Expr::BinOp(binary) if binary.op == ast::Operator::Add => Val::Add(
                Box::new(self.expression(&binary.left, scope)),
                Box::new(self.expression(&binary.right, scope)),
            ),
            Expr::ListComp(ast::ExprListComp {
                elt, generators, ..
            })
            | Expr::Generator(ast::ExprGenerator {
                elt, generators, ..
            }) => {
                let node = match expression {
                    Expr::ListComp(comprehension) => {
                        NodeWithScopeRef::ListComprehension(comprehension)
                    }
                    Expr::Generator(generator) => NodeWithScopeRef::GeneratorExpression(generator),
                    _ => return Val::Opaque(expression.range()),
                };
                self.comprehension(node, elt, generators, scope, true, expression.range())
            }
            Expr::SetComp(comprehension) => self.comprehension(
                NodeWithScopeRef::SetComprehension(comprehension),
                &comprehension.elt,
                &comprehension.generators,
                scope,
                false,
                expression.range(),
            ),
            Expr::If(if_expression) => Val::Either(
                if_expression.start(),
                Box::new([
                    self.expression(&if_expression.body, scope),
                    self.expression(&if_expression.orelse, scope),
                ]),
            ),
            Expr::StringLiteral(string) => Val::Str(Name::new(string.value.to_str())),
            Expr::Subscript(_) => Val::Opaque(expression.range()),
            Expr::Named(named) => self.expression(&named.value, scope),
            _ => Val::Foreign(None),
        }
    }

    fn comprehension(
        &mut self,
        node: NodeWithScopeRef,
        element: &Expr,
        generators: &[ast::Comprehension],
        scope: ScopeId<'db>,
        ordered: bool,
        range: TextRange,
    ) -> Val<'db> {
        let [generator] = generators else {
            return Val::Opaque(range);
        };
        if generator.is_async {
            return Val::Opaque(range);
        }
        let Some(inner) = self.index.try_node_scope(node) else {
            return Val::Opaque(range);
        };
        let inner = inner.to_scope_id(self.db, self.file);
        // The first iterable is evaluated in the enclosing scope.
        let iterable = self.expression(&generator.iter, scope);
        Val::Comprehension {
            element: Box::new(self.expression(element, inner)),
            target: assignment_target(&generator.target),
            iterable: Box::new(iterable),
            filtered: !generator.ifs.is_empty() || !ordered,
        }
    }

    fn call(&mut self, call: &ast::ExprCall, scope: ScopeId<'db>) -> Val<'db> {
        let callee = self.expression(&call.func, scope);
        let arguments: Box<[Val<'db>]> = call
            .arguments
            .args
            .iter()
            .map(|argument| self.expression(argument, scope))
            .collect();
        let mut keywords = Vec::new();
        for keyword in &call.arguments.keywords {
            let value = self.expression(&keyword.value, scope);
            match &keyword.arg {
                Some(name) => keywords.push((name.id.clone(), value)),
                // `**mapping`: mark the call's arguments as unknown.
                None => keywords.push((Name::new_static("**"), value)),
            }
        }
        if self.current.is_some() {
            let db = self.db;
            // The callee and the class are usually module-level names that only the walk binds
            // (`bpy.utils.register_class(C)`), so the facts follow their definitions instead.
            let registers = matches!(callee, Val::Blender(BlenderFunction::RegisterClass))
                || matches!(
                    resolve_expression(db, scope, &call.func),
                    Some(Resolved::Function(definition))
                        if matches!(function_value(db, definition), Val::Blender(BlenderFunction::RegisterClass))
                );
            let named_class = match arguments.first() {
                Some(Val::Class(class, _)) => Some(*class),
                _ => call.arguments.args.first().and_then(|argument| {
                    match resolve_expression(db, scope, argument) {
                        Some(Resolved::Value(Type::ClassLiteral(class))) => class.as_static(),
                        _ => None,
                    }
                }),
            };
            if let Some(facts) = &mut self.current {
                if registers {
                    facts.registers = true;
                    facts.named_classes.extend(named_class);
                }
                if let Val::Global(file, name, _) = &callee {
                    facts.global_callees.push((*file, name.clone()));
                }
            }
        }
        Val::Call {
            callee: Box::new(callee),
            arguments,
            keywords: keywords.into_boxed_slice(),
            range: call.range(),
        }
    }

    /// Whether `name` in `scope` is the walk's to bind: a parameter, loop variable or other local
    /// of a function or comprehension, or a variable of an enclosing function.
    fn is_local(&self, scope: ScopeId<'db>, name: &str) -> bool {
        let file_scope = scope.file_scope_id(self.db);
        let table = self.index.place_table(file_scope);
        table.symbol_id(name).is_some_and(|symbol| {
            !self
                .index
                .symbol_resolves_to_global_scope(symbol, file_scope)
        })
    }

    /// Whether `name` in `scope` is a module-level name of this file.
    fn is_module_name(&self, scope: ScopeId<'db>, name: &str) -> bool {
        let file_scope = scope.file_scope_id(self.db);
        let table = self.index.place_table(file_scope);
        table.symbol_id(name).is_some_and(|symbol| {
            self.index
                .symbol_resolves_to_global_scope(symbol, file_scope)
        })
    }

    fn name(&self, scope: ScopeId<'db>, name: &str, range: TextRange) -> Val<'db> {
        if name == "__name__" {
            return file_to_module(self.db, self.file.resolver_file(self.db))
                .map_or(Val::Foreign(None), |module| {
                    Val::Str(Name::new(module.name(self.db).as_str()))
                });
        }
        if self.is_local(scope, name) {
            return Val::Local(Name::new(name), range);
        }
        module_name_value(self.db, self.file, name, range)
    }

    /// The module-level names that assignments bind, with their entries: the bindings visible at
    /// the end of the module, an augmented assignment adding to the bindings before it, then the
    /// mutations found anywhere in the file.
    fn globals(&mut self) -> Box<[GlobalName<'db>]> {
        let db = self.db;
        let scope = global_scope(db, self.file);
        let table = place_table(db, scope);
        let use_def = use_def_map(db, scope);
        let mut names = std::mem::take(&mut self.assigned);
        names.extend(self.mutations.iter().map(|(name, _)| name.clone()));
        names.sort();
        names.dedup();
        let mut globals = Vec::new();
        for name in names {
            let mut definitions = Vec::new();
            if let Some(symbol) = table.symbol_id(&name) {
                definitions.extend(
                    use_def
                        .end_of_scope_symbol_bindings(symbol)
                        .filter_map(|binding| binding.binding.definition()),
                );
            }
            let mut entries = Vec::new();
            let mut seen = FxHashSet::default();
            while let Some(definition) = definitions.pop() {
                if !seen.insert(definition) {
                    continue;
                }
                if let DefinitionKind::AugmentedAssignment(_) = definition.kind(db) {
                    definitions.extend(
                        use_def
                            .bindings_at_definition(definition)
                            .filter_map(|binding| binding.binding.definition()),
                    );
                }
                if let Some(entry) = self.global_entry(definition) {
                    entries.push(entry);
                }
            }
            entries.sort_by_key(|(start, _)| *start);
            let mut entries: Vec<GlobalEntry<'db>> =
                entries.into_iter().map(|(_, entry)| entry).collect();
            entries.extend(
                self.mutations
                    .iter()
                    .filter(|(mutated, _)| *mutated == name)
                    .map(|(_, entry)| entry.clone()),
            );
            globals.push(GlobalName {
                name,
                entries: entries.into_boxed_slice(),
            });
        }
        globals.into_boxed_slice()
    }

    /// The entry of a module-level binding, with its position in the file.
    fn global_entry(
        &mut self,
        definition: Definition<'db>,
    ) -> Option<(TextSize, GlobalEntry<'db>)> {
        let db = self.db;
        let module = self.module;
        let scope = definition.scope(db);
        let (start, kind) = match definition.kind(db) {
            DefinitionKind::Assignment(assignment) => {
                let value = assignment.value(module);
                let name = assignment.target(module);
                let index = match assignment.unpack() {
                    Some(unpack) => unpack_index(unpack.target(db, module), name.range()),
                    None => Some(Box::default()),
                };
                let kind = match index {
                    Some(index) => EntryKind::Bind {
                        value: self.expression(value, scope),
                        index,
                    },
                    None => EntryKind::Opaque(name.range()),
                };
                (value.start(), kind)
            }
            DefinitionKind::AnnotatedAssignment(assignment) => {
                let value = assignment.value(module)?;
                (
                    value.start(),
                    EntryKind::Bind {
                        value: self.expression(value, scope),
                        index: Box::default(),
                    },
                )
            }
            DefinitionKind::AugmentedAssignment(assignment) => {
                let assignment = assignment.node(module);
                (
                    assignment.value.start(),
                    EntryKind::Extend(self.expression(&assignment.value, scope)),
                )
            }
            _ => {
                let range = definition.focus_range(db, module).range();
                (range.start(), EntryKind::Opaque(range))
            }
        };
        // The innermost statement: a compound statement comes before the statements it holds.
        let path = self
            .statements
            .iter()
            .rev()
            .find(|(range, _)| range.contains(start))
            .map(|(_, path)| path.clone())
            .unwrap_or_default();
        Some((
            start,
            GlobalEntry {
                kind,
                path,
                exact: true,
            },
        ))
    }
}

/// The names an assignment target binds.
/// What an assignment or a `for` loop binds.
fn assignment_target(target: &Expr) -> Target {
    match target {
        Expr::Name(name) => Target::Name(name.id.clone()),
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. })
            if !elts.iter().any(Expr::is_starred_expr) =>
        {
            Target::Tuple(elts.iter().map(assignment_target).collect())
        }
        _ => Target::Other,
    }
}

fn collect_names(target: &Expr, names: &mut Vec<Name>) {
    match target {
        Expr::Name(name) => names.push(name.id.clone()),
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
            for element in elts {
                collect_names(element, names);
            }
        }
        Expr::Starred(starred) => collect_names(&starred.value, names),
        _ => {}
    }
}

/// The position of the name at `range` in the unpacking target `target`: `[1, 0]` for `b` in
/// `a, (b, c) = ...`. `None` when a starred target comes first.
fn unpack_index(target: &Expr, range: TextRange) -> Option<Box<[u32]>> {
    fn find(target: &Expr, range: TextRange, path: &mut Vec<u32>) -> Option<bool> {
        if target.range() == range {
            return Some(true);
        }
        let (Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. })) =
            target
        else {
            return Some(false);
        };
        for (element, position) in elts.iter().zip(0u32..) {
            if element.is_starred_expr() {
                return None;
            }
            path.push(position);
            if find(element, range, path)? {
                return Some(true);
            }
            path.pop();
        }
        Some(false)
    }
    let mut path = Vec::new();
    find(target, range, &mut path)?.then(|| path.into_boxed_slice())
}

/// The value of the module-level name `name` of `file`.
fn module_name_value<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    name: &str,
    range: TextRange,
) -> Val<'db> {
    let scope = global_scope(db, file);
    match resolve_name_in(db, scope, name) {
        Some(Resolved::Module(module)) => Val::Module(module),
        Some(Resolved::Value(Type::ClassLiteral(class))) => class
            .as_static()
            .map_or(Val::Foreign(None), |class| Val::Class(class, range)),
        Some(Resolved::Value(_)) => Val::Foreign(None),
        Some(Resolved::Function(definition)) => function_value(db, definition),
        Some(Resolved::Constant(definition)) => match assigned_name(db, definition) {
            Some((file, name)) if is_first_party(db, file) => Val::Global(file, name, range),
            _ => Val::Foreign(None),
        },
        None => {
            let definitions = find_symbol_in_scope(db, scope, name);
            if definitions.is_empty() {
                return builtin_value(name);
            }
            if !is_first_party(db, file) {
                return Val::Foreign(None);
            }
            let mut target: Option<(ProgramFile<'db>, Name)> = None;
            for definition in definitions {
                let found = match definition.kind(db) {
                    DefinitionKind::Assignment(_)
                    | DefinitionKind::AugmentedAssignment(_)
                    | DefinitionKind::AnnotatedAssignment(_) => Some((file, Name::new(name))),
                    DefinitionKind::ImportFrom(_) => {
                        let resolved = imported_definitions(db, definition, name);
                        match resolved.as_slice() {
                            [ResolvedDefinition::Definition(imported)] => {
                                assigned_name(db, *imported)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                match (found, &target) {
                    (None, _) => return Val::Opaque(range),
                    (Some(found), Some(target)) if found != *target => {
                        return Val::Opaque(range);
                    }
                    (Some(found), _) => target = Some(found),
                }
            }
            match target {
                Some((file, name)) if is_first_party(db, file) => Val::Global(file, name, range),
                _ => Val::Opaque(range),
            }
        }
    }
}

/// The file and name of an assignment's definition.
fn assigned_name<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<(ProgramFile<'db>, Name)> {
    if !matches!(
        definition.kind(db),
        DefinitionKind::Assignment(_)
            | DefinitionKind::AugmentedAssignment(_)
            | DefinitionKind::AnnotatedAssignment(_)
    ) {
        return None;
    }
    let ScopedPlaceId::Symbol(symbol) = definition.place(db) else {
        return None;
    };
    if definition.scope(db).scope(db).kind() != ScopeKind::Module {
        return None;
    }
    let table = place_table(db, definition.scope(db));
    Some((
        definition.program_file(db),
        table.symbol(symbol).name().clone(),
    ))
}

/// A function definition: a first-party function to follow, or one of Blender's or Python's.
fn function_value<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Val<'db> {
    let file = definition.program_file(db);
    if is_first_party(db, file) {
        return Val::Function(definition);
    }
    let Some(module) = file_to_module(db, file.resolver_file(db)) else {
        return Val::Foreign(None);
    };
    let ScopedPlaceId::Symbol(symbol) = definition.place(db) else {
        return Val::Foreign(None);
    };
    let table = place_table(db, definition.scope(db));
    let module = module.name(db).as_str();
    let function = table.symbol(symbol).name().as_str();
    match (module, function) {
        ("bpy.utils", "register_class") => Val::Blender(BlenderFunction::RegisterClass),
        ("bpy.utils", "unregister_class") => Val::Blender(BlenderFunction::UnregisterClass),
        ("bpy.utils", "register_classes_factory") => Val::Blender(BlenderFunction::ClassesFactory),
        ("bpy.utils", "register_submodule_factory") => {
            Val::Blender(BlenderFunction::SubmoduleFactory)
        }
        ("bpy.app.timers", "register") => Val::Blender(BlenderFunction::TimerRegister),
        ("importlib", "import_module")
        | ("inspect", "getmembers")
        | ("pkgutil", "iter_modules" | "walk_packages") => Val::Discovery,
        _ => Val::Foreign(Some(format!("{module}.{function}").into_boxed_str())),
    }
}

/// A name no module binds: a builtin.
fn builtin_value<'db>(name: &str) -> Val<'db> {
    match name {
        "setattr" => Val::Blender(BlenderFunction::Setattr),
        "delattr" => Val::Blender(BlenderFunction::Delattr),
        "reversed" => Val::Blender(BlenderFunction::Reversed),
        "list" | "tuple" => Val::Blender(BlenderFunction::Copy),
        "sorted" | "set" | "frozenset" => Val::Blender(BlenderFunction::Unordered),
        "getattr" | "__import__" | "globals" | "vars" | "locals" | "eval" | "exec" => {
            Val::Discovery
        }
        _ => Val::Foreign(None),
    }
}

/// Whether the module-level name `name` of `file` is bound to the register half of a
/// `register_classes_factory` or `register_submodule_factory` call.
fn is_register_half<'db>(db: &'db dyn Db, file: ProgramFile<'db>, name: &str) -> bool {
    registration_ir(db, file)
        .global(name)
        .is_some_and(|global| {
            global.entries.iter().any(|entry| {
                matches!(
                    &entry.kind,
                    EntryKind::Bind {
                        value: Val::Call { callee, .. },
                        index,
                    } if matches!(
                        **callee,
                        Val::Blender(
                            BlenderFunction::ClassesFactory | BlenderFunction::SubmoduleFactory
                        )
                    ) && **index == [0]
                )
            })
        })
}

// The interpreter.

/// A value while the walk runs.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value<'db> {
    Module(ProgramFile<'db>),
    Class(StaticClassLiteral<'db>, Site),
    Function(Definition<'db>),
    Blender(BlenderFunction),
    Discovery,
    Str(Name),
    Seq(Rc<[Element<'db>]>),
    /// What a factory returned: both functions (`half` is `None`) or one of them.
    Factory(Rc<Factory<'db>>, Option<Half>),
    Foreign(Option<Rc<str>>),
    Opaque(Site),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Element<'db> {
    value: Value<'db>,
    path: Vec<(BranchId, u8)>,
    exact: bool,
}

impl<'db> Element<'db> {
    fn plain(value: Value<'db>) -> Self {
        Element {
            value,
            path: Vec::new(),
            exact: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Factory<'db> {
    Classes(Rc<[Element<'db>]>),
    Submodules {
        package: Name,
        modules: Box<[Name]>,
        /// The module that called the factory, which resolves the names.
        file: ProgramFile<'db>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Half {
    Register,
    Unregister,
}

/// One activation of a body: a module body or a function call.
#[derive(Clone)]
struct Frame<'db> {
    file: ProgramFile<'db>,
    locals: FxHashMap<Name, Value<'db>>,
    path: Vec<(BranchId, u8)>,
    exact: bool,
    module_level: bool,
    returns: Vec<Value<'db>>,
}

impl<'db> Frame<'db> {
    fn new(file: ProgramFile<'db>, module_level: bool) -> Self {
        Frame {
            file,
            locals: FxHashMap::default(),
            path: Vec::new(),
            exact: true,
            module_level,
            returns: Vec::new(),
        }
    }

    fn site(&self, db: &dyn Db, range: TextRange) -> Site {
        Site {
            file: self.file.file(db),
            range,
        }
    }
}

/// How many nested calls the walk follows before it gives up.
const MAX_DEPTH: usize = 64;

struct Walker<'db> {
    db: &'db dyn Db,
    events: Vec<Event<'db>>,
    entered: Vec<Definition<'db>>,
    incomplete: Option<Site>,
    imported: FxHashSet<ProgramFile<'db>>,
    stack: Vec<(Definition<'db>, Vec<Value<'db>>)>,
    escaped: Vec<Definition<'db>>,
    next_branch: u32,
    /// False while the walk runs the functions Blender calls later.
    exact: bool,
    /// Above zero while a module-level name's value is evaluated: what that runs happens elsewhere
    /// (at the module's import), so it records nothing.
    pure: u32,
    globals: FxHashMap<(ProgramFile<'db>, Name), Value<'db>>,
    evaluating: FxHashSet<(ProgramFile<'db>, Name)>,
}

impl<'db> Walker<'db> {
    fn new(db: &'db dyn Db) -> Self {
        Walker {
            db,
            events: Vec::new(),
            entered: Vec::new(),
            incomplete: None,
            imported: FxHashSet::default(),
            stack: Vec::new(),
            escaped: Vec::new(),
            next_branch: 0,
            exact: true,
            pure: 0,
            globals: FxHashMap::default(),
            evaluating: FxHashSet::default(),
        }
    }

    fn finish(self) -> Walk<'db> {
        Walk {
            events: self.events.into_boxed_slice(),
            entered: self.entered.into_boxed_slice(),
            incomplete: self.incomplete,
        }
    }

    fn run(&mut self, root: ProgramFile<'db>, entry: Entry) {
        let db = self.db;
        self.import(root);
        let site = Site {
            file: root.file(db),
            range: TextRange::default(),
        };
        let frame = Frame::new(root, true);
        let function = self.eval(
            &module_name_value(db, root, entry.name(), site.range),
            &frame,
        );
        match function {
            Value::Function(_) | Value::Factory(_, Some(_)) => {
                let mut frame = frame;
                self.call_value(&function, &[], &[], site.range, &mut frame);
            }
            _ => self.give_up(site),
        }
        // Functions handed to Blender run after `register()` returns, in an order the walk does
        // not know.
        self.exact = false;
        let mut walked = FxHashSet::default();
        while let Some(function) = self.escaped.pop() {
            if walked.insert(function) {
                let mut frame = Frame::new(function.program_file(db), false);
                self.call_function(function, Vec::new(), &[], site.range, &mut frame);
            }
        }
    }

    fn give_up(&mut self, site: Site) {
        if self.pure == 0 && self.incomplete.is_none() {
            self.incomplete = Some(site);
        }
    }

    fn record(&mut self, event: Event<'db>) {
        if self.pure == 0 {
            self.events.push(event);
        }
    }

    /// Run the body of `module` at its first import.
    fn import(&mut self, module: ProgramFile<'db>) {
        if !is_first_party(self.db, module) || !self.imported.insert(module) {
            return;
        }
        let ir = registration_ir(self.db, module);
        let mut frame = Frame::new(module, true);
        self.body(&ir.module, &mut frame);
    }

    /// Run `steps`; `true` when a `return` ended them.
    fn body(&mut self, steps: &[Step<'db>], frame: &mut Frame<'db>) -> bool {
        for step in steps {
            if self.step(step, frame) {
                return true;
            }
        }
        false
    }

    fn step(&mut self, step: &Step<'db>, frame: &mut Frame<'db>) -> bool {
        let db = self.db;
        match step {
            Step::Eval(value) => {
                self.eval(value, frame);
            }
            Step::Assign(target, value) => {
                let value = self.eval(value, frame);
                bind(db, target, value, frame);
            }
            Step::SetProperty {
                owner,
                owner_range,
                name,
                value,
                site,
            } => {
                let owner = self.eval(owner, frame);
                let name = self.eval(name, frame);
                match (owner, name) {
                    (Value::Class(owner, _), Value::Str(name)) if is_bpy_types_class(db, owner) => {
                        let event = Event::SetProperty {
                            property: RegisteredProperty {
                                owner,
                                owner_range: *owner_range,
                                accepted: accepts_registered_properties(db, owner),
                                name,
                                value: value.clone(),
                            },
                            site: frame.site(db, *site),
                            path: BranchPath(frame.path.clone().into_boxed_slice()),
                            exact: frame.exact && self.exact,
                        };
                        self.record(event);
                    }
                    (Value::Opaque(site), _) => self.give_up(site),
                    _ => {}
                }
            }
            Step::DelProperty { owner, name } => {
                if let Value::Class(owner, _) = self.eval(owner, frame) {
                    self.record(Event::DelProperty {
                        owner,
                        name: name.clone(),
                    });
                }
            }
            Step::Import(module) => self.import(*module),
            Step::For {
                target,
                iterable,
                body,
            } => {
                let iterable = self.eval(iterable, frame);
                let elements = match iterable {
                    Value::Seq(elements) => elements,
                    Value::Opaque(_) | Value::Discovery => Rc::from([Element::plain(iterable)]),
                    _ => Rc::from([Element::plain(Value::Foreign(None))]),
                };
                for element in elements.iter() {
                    let (depth, exact) = (frame.path.len(), frame.exact);
                    frame.path.extend(element.path.iter().copied());
                    frame.exact &= element.exact;
                    bind(db, target, element.value.clone(), frame);
                    let returned = self.body(body, frame);
                    frame.path.truncate(depth);
                    frame.exact = exact;
                    if returned {
                        return true;
                    }
                }
            }
            Step::Branch { offset, arms } => {
                let id = if frame.module_level {
                    BranchId::Static {
                        file: frame.file.file(db),
                        offset: *offset,
                    }
                } else {
                    self.next_branch += 1;
                    BranchId::Dynamic(self.next_branch)
                };
                let mut all_returned = true;
                for (steps, arm) in arms.iter().zip(0u8..) {
                    frame.path.push((id, arm));
                    all_returned &= self.body(steps, frame);
                    frame.path.pop();
                }
                return all_returned;
            }
            Step::Return(value) => {
                let value = self.eval(value, frame);
                frame.returns.push(value);
                return true;
            }
        }
        false
    }

    fn eval(&mut self, value: &Val<'db>, frame: &Frame<'db>) -> Value<'db> {
        let db = self.db;
        match value {
            Val::Module(module) => Value::Module(*module),
            Val::Class(class, range) => Value::Class(*class, frame.site(db, *range)),
            Val::Function(definition) => Value::Function(*definition),
            Val::Blender(function) => Value::Blender(*function),
            Val::Discovery => Value::Discovery,
            Val::Local(name, range) => frame
                .locals
                .get(name)
                .cloned()
                .unwrap_or_else(|| Value::Opaque(frame.site(db, *range))),
            Val::Global(file, name, range) => self.global(*file, name, frame.site(db, *range)),
            Val::Str(name) => Value::Str(name.clone()),
            Val::Display { elements, ordered } => {
                let mut values = Vec::new();
                for element in elements {
                    if let Val::Starred(inner) = element {
                        values.extend(self.elements(inner, frame).iter().cloned());
                    } else {
                        let mut element = Element::plain(self.eval(element, frame));
                        element.exact = *ordered;
                        values.push(element);
                    }
                }
                Value::Seq(values.into())
            }
            Val::Starred(inner) => self.eval(inner, frame),
            Val::Add(left, right) => {
                let left = self.eval(left, frame);
                let right = self.eval(right, frame);
                match (left, right) {
                    (Value::Seq(left), Value::Seq(right)) => {
                        Value::Seq(left.iter().chain(right.iter()).cloned().collect())
                    }
                    (Value::Opaque(site), _) | (_, Value::Opaque(site)) => Value::Opaque(site),
                    _ => Value::Foreign(None),
                }
            }
            Val::Call {
                callee,
                arguments,
                keywords,
                range,
            } => {
                let mut frame = frame.clone();
                self.call(callee, arguments, keywords, *range, &mut frame)
            }
            Val::Attribute(base, name, range) => {
                let base = self.eval(base, frame);
                self.attribute(base, name, frame.site(db, *range))
            }
            Val::Comprehension {
                element,
                target,
                iterable,
                filtered,
            } => match self.eval(iterable, frame) {
                Value::Seq(elements) => {
                    let mut values = Vec::new();
                    for item in elements.iter() {
                        let mut inner = frame.clone();
                        bind(db, target, item.value.clone(), &mut inner);
                        values.push(Element {
                            value: self.eval(element, &inner),
                            path: item.path.clone(),
                            exact: item.exact && !filtered,
                        });
                    }
                    Value::Seq(values.into())
                }
                Value::Opaque(site) => Value::Opaque(site),
                _ => Value::Foreign(None),
            },
            Val::Either(offset, values) => {
                let values: Vec<Value<'db>> =
                    values.iter().map(|value| self.eval(value, frame)).collect();
                if values.iter().all(|value| matches!(value, Value::Seq(_))) {
                    let id = BranchId::Static {
                        file: frame.file.file(db),
                        offset: *offset,
                    };
                    let mut elements = Vec::new();
                    for (value, arm) in values.iter().zip(0u8..) {
                        if let Value::Seq(items) = value {
                            elements.extend(items.iter().map(|item| {
                                let mut item = item.clone();
                                item.path.insert(0, (id, arm));
                                item
                            }));
                        }
                    }
                    Value::Seq(elements.into())
                } else if values.windows(2).all(|pair| pair[0] == pair[1]) {
                    values.into_iter().next().unwrap_or(Value::Foreign(None))
                } else {
                    Value::Opaque(frame.site(db, TextRange::empty(*offset)))
                }
            }
            Val::Foreign(path) => Value::Foreign(path.as_deref().map(Rc::from)),
            Val::Opaque(range) => Value::Opaque(frame.site(db, *range)),
        }
    }

    /// The elements of `value` used as a sequence (`*value`).
    fn elements(&mut self, value: &Val<'db>, frame: &Frame<'db>) -> Rc<[Element<'db>]> {
        match self.eval(value, frame) {
            Value::Seq(elements) => elements,
            other @ Value::Opaque(_) => Rc::from([Element::plain(other)]),
            _ => Rc::from([]),
        }
    }

    /// The value of the module-level name `name` of `file`: its bindings, then its mutations.
    fn global(&mut self, file: ProgramFile<'db>, name: &Name, site: Site) -> Value<'db> {
        let key = (file, name.clone());
        if let Some(value) = self.globals.get(&key) {
            return value.clone();
        }
        if !self.evaluating.insert(key.clone()) {
            return Value::Opaque(site);
        }
        let db = self.db;
        let ir = registration_ir(db, file);
        let value = match ir.global(name) {
            None => Value::Opaque(site),
            Some(global) => {
                self.pure += 1;
                let frame = Frame::new(file, true);
                let file_of = file.file(db);
                // The value of a binding that is not a sequence, when all such bindings agree.
                let mut single: Option<Value<'db>> = None;
                let mut is_sequence = false;
                let mut elements: Vec<Element<'db>> = Vec::new();
                // Where a binding's value is unknown or disagrees with another.
                let mut unknown: Option<Site> = None;
                for entry in &global.entries {
                    let path: Vec<(BranchId, u8)> = entry
                        .path
                        .iter()
                        .map(|(offset, arm)| {
                            (
                                BranchId::Static {
                                    file: file_of,
                                    offset: *offset,
                                },
                                *arm,
                            )
                        })
                        .collect();
                    let adjust = |items: &[Element<'db>]| {
                        items
                            .iter()
                            .map(|item| Element {
                                value: item.value.clone(),
                                path: path.iter().chain(item.path.iter()).copied().collect(),
                                exact: item.exact && entry.exact,
                            })
                            .collect::<Vec<_>>()
                    };
                    match &entry.kind {
                        EntryKind::Bind { value, index } => {
                            let mut value = self.eval(value, &frame);
                            for position in index {
                                value = match value {
                                    Value::Factory(factory, None) => Value::Factory(
                                        factory,
                                        match position {
                                            0 => Some(Half::Register),
                                            _ => Some(Half::Unregister),
                                        },
                                    ),
                                    Value::Seq(items) => items
                                        .get(*position as usize)
                                        .map_or(Value::Opaque(site), |item| item.value.clone()),
                                    _ => Value::Opaque(site),
                                };
                            }
                            match value {
                                Value::Seq(items) => {
                                    is_sequence = true;
                                    elements.extend(adjust(&items));
                                }
                                Value::Opaque(opaque) => unknown = Some(opaque),
                                other => match &single {
                                    None => single = Some(other),
                                    Some(existing) if *existing == other => {}
                                    Some(_) => unknown = Some(site),
                                },
                            }
                        }
                        EntryKind::Extend(value) => {
                            is_sequence = true;
                            let items = self.elements(value, &frame);
                            elements.extend(adjust(&items));
                        }
                        EntryKind::Append(value) => {
                            is_sequence = true;
                            let item = Element::plain(self.eval(value, &frame));
                            elements.extend(adjust(&[item]));
                        }
                        EntryKind::Opaque(range) => {
                            unknown = Some(Site {
                                file: file_of,
                                range: *range,
                            });
                        }
                    }
                }
                self.pure -= 1;
                match (is_sequence, single, unknown) {
                    (true, None, None) => Value::Seq(elements.into()),
                    // `classes = discover(); classes.append(C)`: the unknown part stays.
                    (true, _, unknown) => {
                        elements.insert(0, Element::plain(Value::Opaque(unknown.unwrap_or(site))));
                        Value::Seq(elements.into())
                    }
                    (false, Some(value), None) => value,
                    (false, _, unknown) => Value::Opaque(unknown.unwrap_or(site)),
                }
            }
        };
        self.evaluating.remove(&key);
        self.globals.insert(key, value.clone());
        value
    }

    /// `base.name`.
    fn attribute(&mut self, base: Value<'db>, name: &Name, site: Site) -> Value<'db> {
        let db = self.db;
        match base {
            Value::Module(module) => {
                let value = module_member(db, module, name, site.range);
                let frame = Frame::new(module, true);
                // A class or global named through the module was named at `site`.
                match value {
                    Val::Class(class, _) => Value::Class(class, site),
                    Val::Global(file, name, _) => self.global(file, &name, site),
                    other => self.eval(&other, &frame),
                }
            }
            Value::Class(class, _) => {
                if is_first_party(db, class.program_file(db)) {
                    let definitions = find_symbol_in_scope(db, class.body_scope(db), name);
                    match definitions.iter().last() {
                        Some(definition)
                            if matches!(definition.kind(db), DefinitionKind::Function(_)) =>
                        {
                            Value::Function(*definition)
                        }
                        _ => Value::Foreign(None),
                    }
                } else {
                    let class_path = bpy_types_path(db, class);
                    Value::Foreign(class_path.map(|path| Rc::from(format!("{path}.{name}"))))
                }
            }
            Value::Foreign(Some(path)) => Value::Foreign(Some(Rc::from(format!("{path}.{name}")))),
            Value::Opaque(site) => Value::Opaque(site),
            Value::Discovery => Value::Opaque(site),
            _ => Value::Foreign(None),
        }
    }

    fn call(
        &mut self,
        callee: &Val<'db>,
        arguments: &[Val<'db>],
        keywords: &[(Name, Val<'db>)],
        range: TextRange,
        frame: &mut Frame<'db>,
    ) -> Value<'db> {
        let db = self.db;
        let callee = if let Val::Attribute(base, method, attribute_range) = callee {
            let base = self.eval(base, frame);
            // `handlers.load_post.append(f)`, `MyMenu.remove(f)`: a hook, not a call.
            if matches!(method.as_str(), "append" | "prepend" | "remove")
                && let [argument] = arguments
                && let Some(target) = hook_target(db, &base)
            {
                if let Value::Function(function) = self.eval(argument, frame) {
                    let added = method.as_str() != "remove";
                    if added
                        && matches!(&target, HookTarget::Path(path) if path.starts_with("bpy.app.handlers."))
                        && self.pure == 0
                    {
                        self.escaped.push(function);
                    }
                    self.record(Event::Hook {
                        target,
                        function,
                        added,
                        site: frame.site(db, range),
                    });
                }
                return Value::Foreign(None);
            }
            self.attribute(base, method, frame.site(db, *attribute_range))
        } else {
            self.eval(callee, frame)
        };
        let mut values = Vec::new();
        let mut unknown_arity = false;
        for argument in arguments {
            if let Val::Starred(inner) = argument {
                unknown_arity = true;
                self.eval(inner, frame);
            } else {
                values.push(self.eval(argument, frame));
            }
        }
        let mut named = Vec::new();
        for (name, value) in keywords {
            let value = self.eval(value, frame);
            if name.as_str() == "**" {
                unknown_arity = true;
            } else {
                named.push((name.clone(), value));
            }
        }
        if unknown_arity && let Value::Function(_) = callee {
            // The parameters are not known: they are all opaque.
            values.clear();
            named.clear();
        }
        self.call_value(&callee, &values, &named, range, frame)
    }

    fn call_value(
        &mut self,
        callee: &Value<'db>,
        arguments: &[Value<'db>],
        keywords: &[(Name, Value<'db>)],
        range: TextRange,
        frame: &mut Frame<'db>,
    ) -> Value<'db> {
        let db = self.db;
        let site = frame.site(db, range);
        match callee {
            Value::Function(definition) => {
                self.call_function(*definition, arguments.to_vec(), keywords, range, frame)
            }
            Value::Blender(function) => {
                let first = arguments.first();
                match function {
                    BlenderFunction::RegisterClass | BlenderFunction::UnregisterClass => {
                        let register = *function == BlenderFunction::RegisterClass;
                        match first {
                            Some(Value::Class(class, named_at)) => {
                                self.class_event(register, *class, *named_at, frame, &[], true);
                            }
                            Some(Value::Opaque(site)) => self.give_up(*site),
                            None => self.give_up(site),
                            _ => {}
                        }
                        Value::Foreign(None)
                    }
                    BlenderFunction::ClassesFactory => match first {
                        Some(Value::Seq(elements)) => {
                            Value::Factory(Rc::new(Factory::Classes(elements.clone())), None)
                        }
                        _ => {
                            self.give_up(site);
                            Value::Opaque(site)
                        }
                    },
                    BlenderFunction::SubmoduleFactory => match (first, arguments.get(1)) {
                        (Some(Value::Str(package)), Some(Value::Seq(names)))
                            if names.iter().all(|name| matches!(name.value, Value::Str(_))) =>
                        {
                            let modules = names
                                .iter()
                                .filter_map(|name| match &name.value {
                                    Value::Str(name) => Some(name.clone()),
                                    _ => None,
                                })
                                .collect();
                            Value::Factory(
                                Rc::new(Factory::Submodules {
                                    package: package.clone(),
                                    modules,
                                    file: frame.file,
                                }),
                                None,
                            )
                        }
                        _ => {
                            self.give_up(site);
                            Value::Opaque(site)
                        }
                    },
                    BlenderFunction::TimerRegister => {
                        if let Some(Value::Function(function)) = first
                            && self.pure == 0
                        {
                            self.escaped.push(*function);
                        }
                        Value::Foreign(None)
                    }
                    BlenderFunction::Delattr => {
                        if let (Some(Value::Class(owner, _)), Some(Value::Str(name))) =
                            (first, arguments.get(1))
                        {
                            self.record(Event::DelProperty {
                                owner: *owner,
                                name: name.clone(),
                            });
                        }
                        Value::Foreign(None)
                    }
                    BlenderFunction::Setattr => Value::Foreign(None),
                    BlenderFunction::Reversed => match first {
                        Some(Value::Seq(elements)) => {
                            Value::Seq(elements.iter().rev().cloned().collect())
                        }
                        Some(other @ Value::Opaque(_)) => other.clone(),
                        _ => Value::Foreign(None),
                    },
                    BlenderFunction::Copy => match first {
                        Some(value @ (Value::Seq(_) | Value::Opaque(_))) => value.clone(),
                        None => Value::Seq(Rc::from([])),
                        _ => Value::Foreign(None),
                    },
                    BlenderFunction::Unordered => match first {
                        Some(Value::Seq(elements)) => Value::Seq(
                            elements
                                .iter()
                                .map(|element| Element {
                                    exact: false,
                                    ..element.clone()
                                })
                                .collect(),
                        ),
                        Some(other @ Value::Opaque(_)) => other.clone(),
                        _ => Value::Foreign(None),
                    },
                }
            }
            Value::Factory(factory, Some(half)) => {
                self.call_factory(factory, *half, range, frame);
                Value::Foreign(None)
            }
            Value::Discovery => Value::Opaque(site),
            Value::Opaque(opaque) => {
                self.give_up(*opaque);
                Value::Opaque(*opaque)
            }
            Value::Class(class, _) if is_first_party(db, class.program_file(db)) => {
                Value::Opaque(site)
            }
            _ => Value::Foreign(None),
        }
    }

    fn class_event(
        &mut self,
        register: bool,
        class: StaticClassLiteral<'db>,
        named_at: Site,
        frame: &Frame<'db>,
        element_path: &[(BranchId, u8)],
        element_exact: bool,
    ) {
        let event = ClassEvent {
            class,
            named_at,
            path: BranchPath(frame.path.iter().chain(element_path).copied().collect()),
            exact: frame.exact && element_exact && self.exact,
        };
        self.record(if register {
            Event::RegisterClass(event)
        } else {
            Event::UnregisterClass(event)
        });
    }

    /// Run what `register_classes_factory` or `register_submodule_factory` made, as Blender's own
    /// code does (`bpy/utils/__init__.py`).
    fn call_factory(
        &mut self,
        factory: &Factory<'db>,
        half: Half,
        range: TextRange,
        frame: &mut Frame<'db>,
    ) {
        let db = self.db;
        match factory {
            Factory::Classes(elements) => {
                let register = half == Half::Register;
                let ordered: Vec<&Element<'db>> = if register {
                    elements.iter().collect()
                } else {
                    elements.iter().rev().collect()
                };
                for element in ordered {
                    match &element.value {
                        Value::Class(class, named_at) => self.class_event(
                            register,
                            *class,
                            *named_at,
                            frame,
                            &element.path,
                            element.exact,
                        ),
                        Value::Opaque(site) => self.give_up(*site),
                        _ => {}
                    }
                }
            }
            Factory::Submodules {
                package,
                modules,
                file,
            } => {
                let names: Vec<&Name> = match half {
                    Half::Register => modules.iter().collect(),
                    Half::Unregister => modules.iter().rev().collect(),
                };
                let entry = match half {
                    Half::Register => "register",
                    Half::Unregister => "unregister",
                };
                for name in names {
                    let Some(module) = module_by_name(db, *file, &format!("{package}.{name}"))
                    else {
                        continue;
                    };
                    self.import(module);
                    let function = self.attribute(
                        Value::Module(module),
                        &Name::new(entry),
                        frame.site(db, range),
                    );
                    self.call_value(&function, &[], &[], range, frame);
                }
            }
        }
    }

    fn call_function(
        &mut self,
        definition: Definition<'db>,
        arguments: Vec<Value<'db>>,
        keywords: &[(Name, Value<'db>)],
        range: TextRange,
        caller: &mut Frame<'db>,
    ) -> Value<'db> {
        let db = self.db;
        let file = definition.program_file(db);
        if !is_first_party(db, file) {
            return Value::Foreign(None);
        }
        let site = caller.site(db, range);
        if self.stack.len() >= MAX_DEPTH {
            self.give_up(site);
            return Value::Opaque(site);
        }
        if self.stack.iter().any(|(active, active_arguments)| {
            *active == definition && *active_arguments == arguments
        }) {
            // Python would recurse forever.
            return Value::Foreign(None);
        }
        self.import(file);
        let ir = registration_ir(db, file);
        let Some(function) = ir.function(definition) else {
            return Value::Opaque(site);
        };
        if self.pure == 0 && !self.entered.contains(&definition) {
            self.entered.push(definition);
        }
        let mut frame = Frame {
            file,
            locals: FxHashMap::default(),
            path: caller.path.clone(),
            exact: caller.exact,
            module_level: false,
            returns: Vec::new(),
        };
        let mut positional = arguments.iter();
        for parameter in &function.parameters {
            let value = match parameter.kind {
                ParameterKind::Variadic => None,
                ParameterKind::Positional => positional.next().cloned(),
                ParameterKind::PositionalOrKeyword => positional.next().cloned().or_else(|| {
                    keywords
                        .iter()
                        .find(|(name, _)| *name == parameter.name)
                        .map(|(_, value)| value.clone())
                }),
                ParameterKind::KeywordOnly => keywords
                    .iter()
                    .find(|(name, _)| *name == parameter.name)
                    .map(|(_, value)| value.clone()),
            };
            let value = value.or_else(|| {
                parameter.default.as_ref().map(|default| {
                    let defaults = Frame::new(file, true);
                    self.eval(default, &defaults)
                })
            });
            if let Some(value) = value {
                frame.locals.insert(parameter.name.clone(), value);
            }
        }
        self.stack.push((definition, arguments));
        self.body(&function.body, &mut frame);
        self.stack.pop();
        let mut returns = frame.returns.into_iter();
        let Some(first) = returns.next() else {
            return Value::Foreign(None);
        };
        returns.fold(first, |merged, value| match (merged, value) {
            (Value::Seq(left), Value::Seq(right)) => {
                Value::Seq(left.iter().chain(right.iter()).cloned().collect())
            }
            (left, right) if left == right => left,
            _ => Value::Opaque(site),
        })
    }
}

/// Bind `target` in `frame` to `value`, element by element for a tuple target.
fn bind<'db>(db: &'db dyn Db, target: &Target, value: Value<'db>, frame: &mut Frame<'db>) {
    match target {
        Target::Name(name) => {
            frame.locals.insert(name.clone(), value);
        }
        Target::Tuple(targets) => match value {
            Value::Seq(elements) if elements.len() == targets.len() => {
                for (target, element) in targets.iter().zip(elements.iter()) {
                    bind(db, target, element.value.clone(), frame);
                }
            }
            Value::Factory(factory, None) if targets.len() == 2 => {
                bind(
                    db,
                    &targets[0],
                    Value::Factory(factory.clone(), Some(Half::Register)),
                    frame,
                );
                bind(
                    db,
                    &targets[1],
                    Value::Factory(factory, Some(Half::Unregister)),
                    frame,
                );
            }
            Value::Foreign(_) => {
                for target in targets {
                    bind(db, target, Value::Foreign(None), frame);
                }
            }
            other => {
                let site = match other {
                    Value::Opaque(site) => site,
                    _ => frame.site(db, TextRange::default()),
                };
                for target in targets {
                    bind(db, target, Value::Opaque(site), frame);
                }
            }
        },
        Target::Other => {}
    }
}

/// `name` of the module `module`, resolved as a module-level name of it, or else as its submodule.
fn module_member<'db>(
    db: &'db dyn Db,
    module: ProgramFile<'db>,
    name: &str,
    range: TextRange,
) -> Val<'db> {
    let defined = !find_symbol_in_scope(db, global_scope(db, module), name).is_empty();
    if defined {
        let value = module_name_value(db, module, name, range);
        if !matches!(value, Val::Foreign(None)) || is_first_party(db, module) {
            return value;
        }
    }
    if let Some(module_name) = file_to_module(db, module.resolver_file(db)) {
        let full = format!("{}.{name}", module_name.name(db));
        if let Some(submodule) = module_by_name(db, module, &full) {
            return Val::Module(submodule);
        }
        if !is_first_party(db, module) {
            return Val::Foreign(Some(full.into_boxed_str()));
        }
    }
    Val::Foreign(None)
}

/// The dotted path of a `bpy.types` class.
fn bpy_types_path<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> Option<String> {
    is_bpy_types_class(db, class).then(|| format!("bpy.types.{}", class.name(db)))
}

/// What a hook added with `base.append(f)` is added to: a list of `bpy.app.handlers`, or a menu,
/// panel or header class.
fn hook_target<'db>(db: &'db dyn Db, base: &Value<'db>) -> Option<HookTarget<'db>> {
    match base {
        Value::Foreign(Some(path))
            if path.starts_with("bpy.app.handlers.")
                || path
                    .strip_prefix("bpy.types.")
                    .is_some_and(|class| !class.contains('.')) =>
        {
            Some(HookTarget::Path(Box::from(&**path)))
        }
        Value::Class(class, _)
            if bpy_types_bases(db, *class)
                .any(|base| matches!(base, "Menu" | "Panel" | "Header")) =>
        {
            Some(HookTarget::Class(*class))
        }
        _ => None,
    }
}
