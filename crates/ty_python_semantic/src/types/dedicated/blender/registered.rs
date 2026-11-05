//! PROTOTYPE (wayfinder ticket #9): registered properties, looked up at the use site.
//!
//! `bpy.types.X.y = IntProperty()` anywhere in an add-on makes `y` an attribute of `X` instances,
//! but only for files of the same add-on. The lookup is keyed on the *accessing* file, never on
//! the class, so two add-ons registering the same name do not see each other.
//!
//! Stand-ins (not the designs from tickets #3 / #8):
//! - discovery: walk up from the file to the first `blender_manifest.toml`, or an `__init__.py`
//!   whose text contains `bl_info`, bounded by the first-party root;
//! - collection: untracked; every lookup re-walks the add-on directory (parsing is cached by salsa);
//! - owner and callee resolved through the registering file's module globals (`global_symbol`)
//!   plus attribute members, so `import bpy` / `from bpy.types import X as Y` work, but names bound
//!   only inside a function do not.

use ruff_db::files::{File, directory_listing, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_db::system::{FileType, SystemPath, SystemPathBuf};
use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::{self as ast, name::Name};
use ty_module_resolver::file_to_module;
use ty_python_core::program_file::ProgramFile;

use crate::Db;
use crate::ProgramEnvironment;
use crate::place::{Place, PlaceAndQualifiers, global_symbol};
use crate::types::attribute_write::{
    AttributeWriteRequirement, ExplicitAttributeWriteRequirement, InstanceAttributeWriteMember,
};
use crate::types::class::ClassLiteral;
use crate::types::list_members::Member;
use crate::types::{KnownClass, Type, TypeQualifiers, UnionType};

/// One registered property: `owner.name = <call>`.
#[derive(Clone)]
struct Registered<'db> {
    /// Name of the `bpy.types` class it is assigned to.
    owner: Name,
    name: Name,
    value_ty: Type<'db>,
}

// ---------------------------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------------------------

/// Hook in `infer_attribute_load_impl`: replace an undefined member with the registered property.
pub(crate) fn on_load<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    value_ty: Type<'db>,
    attr: &str,
    place: PlaceAndQualifiers<'db>,
) -> PlaceAndQualifiers<'db> {
    if !place.place.is_undefined() {
        return place;
    }
    match lookup_for_load(db, env, file, value_ty, attr) {
        Some(ty) => Place::bound(ty).into(),
        None => place,
    }
}

/// Hook in `validate_attribute_assignment`: the write requirement for a registered property,
/// or `None` to let ty decide. Only used when ordinary lookup finds nothing.
pub(crate) fn on_store<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    object_ty: Type<'db>,
    attr: &str,
) -> Option<AttributeWriteRequirement<'db>> {
    if !object_ty.member(db, env, attr).place.is_undefined() {
        return None;
    }
    Some(
        match write_type_for_store(db, env, file, object_ty, attr)? {
            None => AttributeWriteRequirement::Unconstrained,
            Some(ty) => AttributeWriteRequirement::Instance {
                object_ty,
                receiver: None,
                member: InstanceAttributeWriteMember::Explicit {
                    member: ExplicitAttributeWriteRequirement::AssignableTo {
                        ty,
                        qualifiers: TypeQualifiers::empty(),
                    },
                    fallback: None,
                },
            },
        },
    )
}

/// Read hook: called when ordinary attribute lookup failed. Returns the registered property's
/// value type when `value_ty` is a `bpy.types` instance (or class, for `del bpy.types.X.y`) and
/// the accessing file's add-on registers `attr` on its class or a base class.
pub(crate) fn lookup_for_load<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    value_ty: Type<'db>,
    attr: &str,
) -> Option<Type<'db>> {
    let (class, _on_class) = receiver_class(db, env, value_ty)?;
    let props = registered_in_addon_of(db, env, file);
    find(db, class, &props, attr).map(|p| p.value_ty)
}

/// Write hook: `Some(Some(ty))` = instance write, check assignability to `ty`;
/// `Some(None)` = class-level registration site (`bpy.types.X.y = Prop()`), accept;
/// `None` = not a registered property, fall through to ty.
pub(crate) fn write_type_for_store<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    object_ty: Type<'db>,
    attr: &str,
) -> Option<Option<Type<'db>>> {
    let (class, on_class) = receiver_class(db, env, object_ty)?;
    let props = registered_in_addon_of(db, env, file);
    let prop = find(db, class, &props, attr)?;
    Some(if on_class { None } else { Some(prop.value_ty) })
}

/// Completion hook: registered properties visible on `ty` from `file`.
pub(crate) fn completions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    ty: Type<'db>,
) -> Vec<Member<'db>> {
    let Some((class, _)) = receiver_class(db, env, ty) else {
        return Vec::new();
    };
    let props = registered_in_addon_of(db, env, file);
    let mut seen: Vec<&Name> = Vec::new();
    let mut out = Vec::new();
    for prop in &props {
        if seen.contains(&&prop.name) {
            continue;
        }
        if let Some(found) = find(db, class, &props, &prop.name) {
            seen.push(&prop.name);
            out.push(Member {
                name: found.name.clone(),
                ty: found.value_ty,
                is_type_check_only: false,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------------------------

/// The `bpy.types` class behind an instance or class-object receiver.
fn receiver_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<(ClassLiteral<'db>, bool)> {
    let (class, on_class) = match ty {
        Type::NominalInstance(instance) => (instance.class(db, env).class_literal(db), false),
        Type::ClassLiteral(class) => (class, true),
        _ => return None,
    };
    is_bpy_types_class(db, class).then_some((class, on_class))
}

fn is_bpy_types_class(db: &dyn Db, class: ClassLiteral<'_>) -> bool {
    file_to_module(db, class.program_file(db).resolver_file(db))
        .is_some_and(|module| module.name(db).as_str().starts_with("bpy.types"))
}

/// Most-derived owner wins: walk the receiver's MRO and take the first class with a registration.
fn find<'a, 'db>(
    db: &'db dyn Db,
    class: ClassLiteral<'db>,
    props: &'a [Registered<'db>],
    attr: &str,
) -> Option<&'a Registered<'db>> {
    for base in class.iter_mro(db) {
        let Some(base) = base.into_class() else {
            continue;
        };
        let base = base.class_literal(db);
        if !is_bpy_types_class(db, base) {
            continue;
        }
        let base_name = base.name(db);
        // Last registration in walk order wins; good enough for a prototype.
        if let Some(prop) = props
            .iter()
            .rev()
            .find(|p| p.name.as_str() == attr && &p.owner == base_name)
        {
            return Some(prop);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Discovery stand-in
// ---------------------------------------------------------------------------------------------

/// The add-on root containing `file`: a folder (extension / legacy) or the file itself.
fn addon_root(db: &dyn Db, file: ProgramFile<'_>) -> Option<SystemPathBuf> {
    let path = file.file(db).path(db).as_system_path()?;
    let program = file.program(db);
    let first_party = program
        .search_paths(db)
        .first_party_roots()
        .filter(|root| path.starts_with(*root))
        .min_by_key(|root| root.components().count())?;
    for dir in path.parent()?.ancestors() {
        if !dir.starts_with(first_party) {
            break;
        }
        if is_addon_dir(db, dir) {
            return Some(dir.to_path_buf());
        }
    }
    has_bl_info(db, file.file(db)).then(|| path.to_path_buf())
}

fn is_addon_dir(db: &dyn Db, dir: &SystemPath) -> bool {
    system_path_to_file(db, dir.join("blender_manifest.toml")).is_ok()
        || system_path_to_file(db, dir.join("__init__.py")).is_ok_and(|init| has_bl_info(db, init))
}

fn has_bl_info(db: &dyn Db, file: File) -> bool {
    source_text(db, file).contains("bl_info")
}

fn addon_files(db: &dyn Db, root: &SystemPath, out: &mut Vec<File>, is_root: bool) {
    if let Ok(file) = system_path_to_file(db, root)
        && root.extension() == Some("py")
    {
        out.push(file); // single-file add-on
        return;
    }
    if !is_root && is_addon_dir(db, root) {
        return; // nested add-on owns its files
    }
    let Ok(listing) = directory_listing(db, root) else {
        return;
    };
    for (name, kind) in listing.iter() {
        let child = root.join(name);
        match kind {
            FileType::File if name.ends_with(".py") => {
                if let Ok(file) = system_path_to_file(db, &child) {
                    out.push(file);
                }
            }
            FileType::Directory if !name.starts_with('.') && name != "__pycache__" => {
                addon_files(db, &child, out, false);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Collection stand-in
// ---------------------------------------------------------------------------------------------

fn registered_in_addon_of<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
) -> Vec<Registered<'db>> {
    let Some(root) = addon_root(db, file) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    addon_files(db, &root, &mut files, true);
    let program = file.program(db);
    let mut out = Vec::new();
    for f in files {
        collect_in_file(db, env, ProgramFile::new(db, f, program), &mut out);
    }
    out
}

struct Candidates<'a> {
    found: Vec<(&'a ast::Expr, Name, &'a ast::ExprCall)>,
}

impl<'a> StatementVisitor<'a> for Candidates<'a> {
    fn visit_stmt(&mut self, stmt: &'a ast::Stmt) {
        match stmt {
            ast::Stmt::Assign(assign) => {
                if let [ast::Expr::Attribute(target)] = assign.targets.as_slice()
                    && let ast::Expr::Call(call) = &*assign.value
                {
                    self.found
                        .push((&target.value, target.attr.id.clone(), call));
                }
            }
            ast::Stmt::Expr(expr) => {
                if let ast::Expr::Call(call) = &*expr.value
                    && let ast::Expr::Name(func) = &*call.func
                    && func.id.as_str() == "setattr"
                    && let [
                        owner,
                        ast::Expr::StringLiteral(name),
                        ast::Expr::Call(value),
                    ] = &*call.arguments.args
                {
                    self.found
                        .push((owner, Name::new(name.value.to_str()), value));
                }
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

fn collect_in_file<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    out: &mut Vec<Registered<'db>>,
) {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let mut visitor = Candidates { found: Vec::new() };
    visitor.visit_body(parsed.suite());
    for (owner_expr, name, call) in visitor.found {
        let Some(Type::ClassLiteral(owner)) = resolve(db, env, file, owner_expr) else {
            continue;
        };
        if !is_bpy_types_class(db, owner) {
            continue;
        }
        let Some(kind) = property_kind(db, env, file, &call.func) else {
            continue;
        };
        let value_ty = value_type(db, env, file, kind, call, owner);
        out.push(Registered {
            owner: owner.name(db).clone(),
            name,
            value_ty,
        });
    }
}

/// Resolve `a`, `a.b.c` through the file's module globals.
fn resolve<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    expr: &ast::Expr,
) -> Option<Type<'db>> {
    match expr {
        ast::Expr::Name(name) => global_symbol(db, file, &name.id).ignore_possibly_undefined(),
        ast::Expr::Attribute(attr) => resolve(db, env, file, &attr.value)?
            .member(db, env, &attr.attr.id)
            .place
            .ignore_possibly_undefined(),
        _ => None,
    }
}

fn property_kind(
    db: &dyn Db,
    env: &ProgramEnvironment<'_>,
    file: ProgramFile<'_>,
    func: &ast::Expr,
) -> Option<&'static str> {
    // Resolved name first (handles aliases), syntactic name as a fallback.
    let resolved = match resolve(db, env, file, func) {
        Some(Type::FunctionLiteral(function)) => Some(function.name(db).to_string()),
        _ => None,
    };
    let syntactic = match func {
        ast::Expr::Name(name) => Some(name.id.to_string()),
        ast::Expr::Attribute(attr) => Some(attr.attr.id.to_string()),
        _ => None,
    };
    let name = resolved.or(syntactic)?;
    [
        "BoolProperty",
        "IntProperty",
        "FloatProperty",
        "StringProperty",
        "EnumProperty",
        "PointerProperty",
        "CollectionProperty",
    ]
    .into_iter()
    .find(|kind| *kind == name)
}

fn keyword<'a>(call: &'a ast::ExprCall, name: &str) -> Option<&'a ast::Expr> {
    call.arguments
        .keywords
        .iter()
        .find(|k| k.arg.as_ref().is_some_and(|arg| arg.as_str() == name))
        .map(|k| &k.value)
}

fn value_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ProgramFile<'db>,
    kind: &str,
    call: &ast::ExprCall,
    owner: ClassLiteral<'db>,
) -> Type<'db> {
    match kind {
        "BoolProperty" => KnownClass::Bool.to_instance(db, env),
        "IntProperty" => KnownClass::Int.to_instance(db, env),
        // The annotation `float` means `int | float`; match it so `assert_type(x, float)` holds.
        "FloatProperty" => UnionType::from_elements(
            db,
            env,
            [
                KnownClass::Int.to_instance(db, env),
                KnownClass::Float.to_instance(db, env),
            ],
        ),
        "StringProperty" => {
            let bytes = matches!(keyword(call, "subtype"),
                Some(ast::Expr::StringLiteral(s)) if s.value.to_str() == "BYTE_STRING");
            if bytes {
                KnownClass::Bytes.to_instance(db, env)
            } else {
                KnownClass::Str.to_instance(db, env)
            }
        }
        "EnumProperty" => {
            // Static items only, read from the syntax; ENUM_FLAG not handled in the prototype.
            let literals = match keyword(call, "items") {
                Some(ast::Expr::List(ast::ExprList { elts, .. }))
                | Some(ast::Expr::Tuple(ast::ExprTuple { elts, .. })) => elts
                    .iter()
                    .filter(|e| !e.is_none_literal_expr())
                    .map(|e| match e {
                        ast::Expr::Tuple(t) => match t.elts.first() {
                            Some(ast::Expr::StringLiteral(s)) => {
                                Some(Type::string_literal(db, s.value.to_str()))
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>(),
                _ => None,
            };
            match literals {
                Some(literals) if !literals.is_empty() => {
                    UnionType::from_elements(db, env, literals)
                }
                _ => KnownClass::Str.to_instance(db, env),
            }
        }
        "PointerProperty" | "CollectionProperty" => {
            let Some(Type::ClassLiteral(target)) =
                keyword(call, "type").and_then(|t| resolve(db, env, file, t))
            else {
                return Type::unknown();
            };
            let instance = Type::instance(db, env, target.default_specialization(db));
            if kind == "CollectionProperty" {
                // Prototype: unspecialised collection class from the owner's module.
                return match global_symbol(db, owner.program_file(db), "bpy_prop_collection_idprop")
                    .ignore_possibly_undefined()
                {
                    Some(Type::ClassLiteral(coll)) => {
                        Type::instance(db, env, coll.default_specialization(db))
                    }
                    _ => Type::unknown(),
                };
            }
            let is_id = target.iter_mro(db).any(|base| {
                base.into_class().is_some_and(|base| {
                    let base = base.class_literal(db);
                    base.name(db).as_str() == "ID" && is_bpy_types_class(db, base)
                })
            });
            if is_id {
                UnionType::from_elements(db, env, [instance, Type::none(db, env)])
            } else {
                instance
            }
        }
        _ => Type::unknown(),
    }
}
