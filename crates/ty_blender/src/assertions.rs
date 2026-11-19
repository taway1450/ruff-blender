//! A test case's assertions, which stop the test when they fail, as an `assert` statement does.

use ruff_python_ast::{self as ast, Expr};

/// What a test case's assertion statement establishes about its first argument for the rest of
/// the test.
#[derive(Clone, Copy, Debug)]
pub enum UnittestAssertion<'a> {
    /// `self.assertIsNotNone(x)`, or `self.assertNotEqual(x, None)` with `None` on either side:
    /// `x` is not `None`, as after `assert x is not None`.
    IsNotNone(&'a Expr),
    /// `self.assertTrue(x)`: `x` is truthy, as after `assert x`.
    True(&'a Expr),
    /// `self.assertIsNone(x)`: `x` is `None`, as after `assert x is None`.
    IsNone(&'a Expr),
    /// `self.assertFalse(x)`: `x` is falsy, as after `assert not x`.
    False(&'a Expr),
    /// `self.assertIsInstance(x, classinfo)`: `x` is an instance of `classinfo`, as after
    /// `assert isinstance(x, classinfo)`.
    IsInstance(&'a Expr, &'a Expr),
}

/// The assertion `call` makes, when it is one of the test case's own (`self.assertIsNotNone(x)`,
/// `self.assertTrue(x, "message")`). Another object's method of the same name is not taken as one.
pub fn unittest_assertion(call: &ast::ExprCall) -> Option<UnittestAssertion<'_>> {
    let Expr::Attribute(method) = &*call.func else {
        return None;
    };
    if !method
        .value
        .as_name_expr()
        .is_some_and(|receiver| receiver.id == "self")
    {
        return None;
    }
    let target = call.arguments.args.first()?;
    if target.is_starred_expr() {
        return None;
    }
    match method.attr.as_str() {
        "assertIsNotNone" => Some(UnittestAssertion::IsNotNone(target)),
        "assertNotEqual" => {
            let other = call.arguments.args.get(1)?;
            match (target.is_none_literal_expr(), other.is_none_literal_expr()) {
                (false, true) => Some(UnittestAssertion::IsNotNone(target)),
                (true, false) if !other.is_starred_expr() => {
                    Some(UnittestAssertion::IsNotNone(other))
                }
                _ => None,
            }
        }
        "assertTrue" => Some(UnittestAssertion::True(target)),
        "assertIsNone" => Some(UnittestAssertion::IsNone(target)),
        "assertFalse" => Some(UnittestAssertion::False(target)),
        "assertIsInstance" => {
            let classinfo = call.arguments.args.get(1)?;
            (!classinfo.is_starred_expr())
                .then_some(UnittestAssertion::IsInstance(target, classinfo))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::Expr;
    use ruff_python_parser::parse_expression;

    use super::{UnittestAssertion, unittest_assertion};

    fn assertion_kind(source: &str) -> Option<&'static str> {
        let parsed = parse_expression(source).unwrap();
        let Expr::Call(call) = parsed.expr() else {
            return None;
        };
        unittest_assertion(call).map(|assertion| match assertion {
            UnittestAssertion::IsNotNone(_) => "is-not-none",
            UnittestAssertion::True(_) => "true",
            UnittestAssertion::IsNone(_) => "is-none",
            UnittestAssertion::False(_) => "false",
            UnittestAssertion::IsInstance(..) => "is-instance",
        })
    }

    #[test]
    fn assertions() {
        assert_eq!(
            assertion_kind("self.assertIsNotNone(ob)"),
            Some("is-not-none")
        );
        assert_eq!(
            assertion_kind("self.assertIsNotNone(ob.data, 'msg')"),
            Some("is-not-none")
        );
        assert_eq!(assertion_kind("self.assertTrue(ob)"), Some("true"));
        assert_eq!(assertion_kind("other.assertIsNotNone(ob)"), None);
        assert_eq!(assertion_kind("self.assertIsNone(ob)"), Some("is-none"));
        assert_eq!(assertion_kind("self.assertFalse(ob)"), Some("false"));
        assert_eq!(
            assertion_kind("self.assertIsInstance(con, ChildOfConstraint)"),
            Some("is-instance")
        );
        assert_eq!(
            assertion_kind("self.assertIsInstance(con, (A, B), 'msg')"),
            Some("is-instance")
        );
        assert_eq!(assertion_kind("self.assertIsInstance(con)"), None);
        assert_eq!(assertion_kind("self.assertIsInstance(con, *classes)"), None);
        assert_eq!(assertion_kind("self.assertEqual(ob, None)"), None);
        assert_eq!(
            assertion_kind("self.assertNotEqual(ob, None)"),
            Some("is-not-none")
        );
        assert_eq!(
            assertion_kind("self.assertNotEqual(None, ob.data, 'msg')"),
            Some("is-not-none")
        );
        assert_eq!(assertion_kind("self.assertNotEqual(ob, other)"), None);
        assert_eq!(assertion_kind("self.assertNotEqual(None, *args)"), None);
        assert_eq!(assertion_kind("self.assertIsNotNone(*args)"), None);
        assert_eq!(assertion_kind("self.assertTrue()"), None);
    }
}
