//! The script guard: code an add-on runs only when its file runs as a script.
//!
//! ```python
//! if __name__ == "__main__":
//!     register()
//! ```
//!
//! Blender's bundled add-ons end with it, for running the file from the text editor. Blender imports
//! an add-on as a module, so `__name__` is the module's name and the guarded branch never runs.

use ruff_python_ast as ast;

/// Whether `test` is `__name__ == "__main__"`, in either order.
pub fn is_script_test(test: &ast::Expr) -> bool {
    let ast::Expr::Compare(compare) = test else {
        return false;
    };
    let ([ast::CmpOp::Eq], [left, right]) = (&*compare.ops, &*compare.operands) else {
        return false;
    };
    let is_name = |expression: &ast::Expr| {
        expression
            .as_name_expr()
            .is_some_and(|name| name.id.as_str() == "__name__")
    };
    let is_main = |expression: &ast::Expr| {
        expression
            .as_string_literal_expr()
            .is_some_and(|literal| literal.value.to_str() == "__main__")
    };
    (is_name(left) && is_main(right)) || (is_main(left) && is_name(right))
}

#[cfg(test)]
mod tests {
    use ruff_python_parser::parse_expression;

    use super::is_script_test;

    fn script_test(source: &str) -> bool {
        is_script_test(parse_expression(source).unwrap().expr())
    }

    #[test]
    fn either_order() {
        assert!(script_test("__name__ == \"__main__\""));
        assert!(script_test("'__main__' == __name__"));
    }

    #[test]
    fn other_tests() {
        assert!(!script_test("__name__ != \"__main__\""));
        assert!(!script_test("__name__ == \"my_addon\""));
        assert!(!script_test("name == \"__main__\""));
        assert!(!script_test("__name__ == \"__main__\" == x"));
    }
}
