//! The reload guard: Blender's idiom for re-importing a module's submodules when it reloads the module.
//!
//! ```python
//! if "bpy" in locals():
//!     import importlib
//!     importlib.reload(ops)
//! else:
//!     from . import ops
//! ```
//!
//! Blender reloads an add-on (F8, or a changed module on disk) and its own startup scripts (`bl_ui`,
//! `bl_operators`, "Reload Scripts") by running `importlib.reload` on the module, which re-executes
//! the code in the module's old `__dict__`. Only then is `"bpy"` already in `locals()`, so the
//! guarded branch runs with every name of the previous run still bound. The inverted form guards the
//! `else` branch:
//!
//! ```python
//! if "functions" not in locals():
//!     from . import functions
//! else:
//!     functions = importlib.reload(functions)
//! ```
//!
//! A package's submodules are names of the previous run too, even when the package imported them only
//! inside a function: importing `package.ops` (`from . import ops` in `register()`) binds `ops` in
//! the package's `__init__`. Blender's startup packages import theirs by name:
//!
//! ```python
//! _modules = ["space_view3d", "properties_data_mesh"]
//! __import__(name=__name__, fromlist=_modules)
//! ```

use ruff_python_ast as ast;
use ruff_text_size::{Ranged, TextRange};

/// Whether `range` lies in the guarded branch of a reload guard among `body`, the module's top-level
/// statements: the body of `if "<name>" in locals():`, or the `else` branch of
/// `if "<name>" not in locals():`. The other branches don't count: they run on the first import.
pub fn in_reload_guard(body: &[ast::Stmt], range: TextRange) -> bool {
    body.iter().any(|statement| {
        let ast::Stmt::If(guard) = statement else {
            return false;
        };
        let guarded = match reload_test(&guard.test) {
            Some(ast::CmpOp::In) => Some(guard.body.as_slice()),
            Some(ast::CmpOp::NotIn) => match guard.elif_else_clauses.as_slice() {
                [clause] if clause.test.is_none() => Some(clause.body.as_slice()),
                _ => None,
            },
            _ => None,
        };
        guarded.is_some_and(|guarded| spans(guarded).is_some_and(|span| span.contains_range(range)))
    })
}

/// Whether the package `__init__` whose top-level statements are `body` imports its submodule `name`
/// with `__import__(name=__name__, fromlist=<list>)`, where `<list>` is a list of string literals or a
/// module-level name bound to one (and extended with `<name>.append("...")`).
pub fn imports_submodule_by_name(body: &[ast::Stmt], name: &str) -> bool {
    body.iter().any(|statement| {
        let ast::Stmt::Expr(ast::StmtExpr { value, .. }) = statement else {
            return false;
        };
        let Some(fromlist) = dunder_import_fromlist(value) else {
            return false;
        };
        match fromlist {
            ast::Expr::List(list) => list_contains(list, name),
            ast::Expr::Name(list_name) => module_list_contains(body, list_name.id.as_str(), name),
            _ => false,
        }
    })
}

/// The range from the first to the last of `statements`.
fn spans(statements: &[ast::Stmt]) -> Option<TextRange> {
    let (first, last) = (statements.first()?, statements.last()?);
    Some(TextRange::new(first.start(), last.end()))
}

/// The operator of `"<name>" in locals()` or `"<name>" not in locals()`, with any string literal and
/// a `locals()` call without arguments.
fn reload_test(test: &ast::Expr) -> Option<ast::CmpOp> {
    let ast::Expr::Compare(compare) = test else {
        return None;
    };
    let ([op @ (ast::CmpOp::In | ast::CmpOp::NotIn)], [left, comparator]) =
        (&*compare.ops, &*compare.operands)
    else {
        return None;
    };
    if !left.is_string_literal_expr() {
        return None;
    }
    let ast::Expr::Call(call) = comparator else {
        return None;
    };
    (call.arguments.is_empty()
        && call
            .func
            .as_name_expr()
            .is_some_and(|name| name.id.as_str() == "locals"))
    .then_some(*op)
}

/// The `fromlist` of `__import__(name=__name__, fromlist=<fromlist>)`, the package importing its own
/// submodules.
fn dunder_import_fromlist(expr: &ast::Expr) -> Option<&ast::Expr> {
    let ast::Expr::Call(call) = expr else {
        return None;
    };
    if !call
        .func
        .as_name_expr()
        .is_some_and(|func| func.id.as_str() == "__import__")
        || !call.arguments.args.is_empty()
    {
        return None;
    }
    let own_name = call.arguments.find_keyword("name")?.value.as_name_expr()?;
    if own_name.id.as_str() != "__name__" {
        return None;
    }
    Some(&call.arguments.find_keyword("fromlist")?.value)
}

/// Whether the module-level list `list_name` holds the string `name`: in the list literal it is
/// assigned, or in a `list_name.append("...")` call at module level (also under an `if`).
fn module_list_contains(body: &[ast::Stmt], list_name: &str, name: &str) -> bool {
    body.iter().any(|statement| match statement {
        ast::Stmt::Assign(assign) => {
            matches!(&*assign.targets, [ast::Expr::Name(target)] if target.id.as_str() == list_name)
                && matches!(&*assign.value, ast::Expr::List(list) if list_contains(list, name))
        }
        ast::Stmt::AnnAssign(assign) => {
            matches!(&*assign.target, ast::Expr::Name(target) if target.id.as_str() == list_name)
                && matches!(assign.value.as_deref(), Some(ast::Expr::List(list)) if list_contains(list, name))
        }
        ast::Stmt::Expr(ast::StmtExpr { value, .. }) => appends(value, list_name, name),
        ast::Stmt::If(branch) => {
            module_list_contains(&branch.body, list_name, name)
                || branch
                    .elif_else_clauses
                    .iter()
                    .any(|clause| module_list_contains(&clause.body, list_name, name))
        }
        _ => false,
    })
}

/// `list_name.append("<name>")`.
fn appends(expr: &ast::Expr, list_name: &str, name: &str) -> bool {
    let ast::Expr::Call(call) = expr else {
        return false;
    };
    let ast::Expr::Attribute(attribute) = &*call.func else {
        return false;
    };
    attribute.attr.as_str() == "append"
        && attribute
            .value
            .as_name_expr()
            .is_some_and(|target| target.id.as_str() == list_name)
        && matches!(&*call.arguments.args, [ast::Expr::StringLiteral(item)] if item.value.to_str() == name)
}

fn list_contains(list: &ast::ExprList, name: &str) -> bool {
    list.elts
        .iter()
        .any(|item| matches!(item, ast::Expr::StringLiteral(item) if item.value.to_str() == name))
}

#[cfg(test)]
mod tests {
    use ruff_python_parser::parse_module;
    use ruff_text_size::{TextRange, TextSize};

    use super::{imports_submodule_by_name, in_reload_guard};

    fn guarded(source: &str, needle: &str) -> bool {
        let parsed = parse_module(source).unwrap();
        let start = TextSize::try_from(source.find(needle).unwrap()).unwrap();
        let range = TextRange::at(start, TextSize::try_from(needle.len()).unwrap());
        in_reload_guard(&parsed.syntax().body, range)
    }

    fn imported(source: &str, name: &str) -> bool {
        let parsed = parse_module(source).unwrap();
        imports_submodule_by_name(&parsed.syntax().body, name)
    }

    #[test]
    fn guarded_branch() {
        let source = "if \"bpy\" in locals():\n    reload(ops)\nelse:\n    from . import ops\n";
        assert!(guarded(source, "ops)"));
        assert!(!guarded(source, "import ops"));
    }

    #[test]
    fn inverted_guard_guards_else() {
        let source =
            "if \"ops\" not in locals():\n    from . import ops\nelse:\n    ops = reload(ops)\n";
        assert!(guarded(source, "ops)"));
        assert!(!guarded(source, "import ops"));
        let elif = "if \"ops\" not in locals():\n    pass\nelif x:\n    reload(ops)\n";
        assert!(!guarded(elif, "ops)"));
    }

    #[test]
    fn other_tests_are_not_guards() {
        assert!(!guarded(
            "if \"bpy\" in globals():\n    reload(ops)\n",
            "ops)"
        ));
        assert!(!guarded("if bpy in locals():\n    reload(ops)\n", "ops)"));
        assert!(!guarded(
            "if \"bpy\" in locals(x):\n    reload(ops)\n",
            "ops)"
        ));
    }

    #[test]
    fn only_module_level() {
        let source = "def f():\n    if \"bpy\" in locals():\n        reload(ops)\n";
        assert!(!guarded(source, "ops)"));
    }

    #[test]
    fn fromlist_import() {
        let source = "_modules = [\"a\", \"b\"]\nif x:\n    _modules.append(\"c\")\n__import__(name=__name__, fromlist=_modules)\n";
        assert!(imported(source, "a"));
        assert!(imported(source, "c"));
        assert!(!imported(source, "d"));
        assert!(imported(
            "__import__(name=__name__, fromlist=[\"a\"])\n",
            "a"
        ));
        assert!(!imported(
            "__import__(name=\"other\", fromlist=[\"a\"])\n",
            "a"
        ));
        assert!(!imported("_modules = [\"a\"]\n", "a"));
    }
}
