//! The reload guard: Blender's idiom for re-importing an add-on's modules when it reloads the add-on.
//!
//! ```python
//! if "bpy" in locals():
//!     import importlib
//!     importlib.reload(ops)
//! else:
//!     from . import ops
//! ```
//!
//! Blender reloads an add-on (F8, or a changed module on disk) by running `importlib.reload` on its
//! module, which re-executes the code in the module's old `__dict__`. Only then is `"bpy"` already
//! in `locals()`, so the guarded branch runs with every name of the previous run still bound.

use ruff_python_ast as ast;
use ruff_text_size::{Ranged, TextRange};

/// Whether `range` lies in the guarded branch of a reload guard: a module-level
/// `if "<name>" in locals():` among `body`, the module's top-level statements. The `elif` and
/// `else` branches don't count: they run on the first import.
pub fn in_reload_guard(body: &[ast::Stmt], range: TextRange) -> bool {
    body.iter().any(|statement| {
        if let ast::Stmt::If(guard) = statement
            && is_reload_test(&guard.test)
            && let (Some(first), Some(last)) = (guard.body.first(), guard.body.last())
        {
            TextRange::new(first.start(), last.end()).contains_range(range)
        } else {
            false
        }
    })
}

/// `"<name>" in locals()`, with any string literal and a `locals()` call without arguments.
fn is_reload_test(test: &ast::Expr) -> bool {
    let ast::Expr::Compare(compare) = test else {
        return false;
    };
    let ([ast::CmpOp::In], [left, comparator]) = (&*compare.ops, &*compare.operands) else {
        return false;
    };
    if !left.is_string_literal_expr() {
        return false;
    }
    let ast::Expr::Call(call) = comparator else {
        return false;
    };
    call.arguments.is_empty()
        && call
            .func
            .as_name_expr()
            .is_some_and(|name| name.id.as_str() == "locals")
}

#[cfg(test)]
mod tests {
    use ruff_python_parser::parse_module;
    use ruff_text_size::{TextRange, TextSize};

    use super::in_reload_guard;

    fn guarded(source: &str, needle: &str) -> bool {
        let parsed = parse_module(source).unwrap();
        let start = TextSize::try_from(source.find(needle).unwrap()).unwrap();
        let range = TextRange::at(start, TextSize::try_from(needle.len()).unwrap());
        in_reload_guard(&parsed.syntax().body, range)
    }

    #[test]
    fn guarded_branch() {
        let source = "if \"bpy\" in locals():\n    reload(ops)\nelse:\n    from . import ops\n";
        assert!(guarded(source, "ops)"));
        assert!(!guarded(source, "import ops"));
    }

    #[test]
    fn other_tests_are_not_guards() {
        assert!(!guarded(
            "if \"bpy\" in globals():\n    reload(ops)\n",
            "ops)"
        ));
        assert!(!guarded(
            "if \"bpy\" not in locals():\n    reload(ops)\n",
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
}
