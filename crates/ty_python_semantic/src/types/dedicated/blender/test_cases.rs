//! Whether a value is a `unittest.TestCase`, whose assertions stop the test when they fail.

use crate::Db;
use crate::ProgramEnvironment;
use crate::types::Type;

use super::vector::module_class;

/// Whether `ty` is an instance of `unittest.TestCase` or a subclass, so that its `assertIsNotNone`
/// raises when the value is None. A class that only has a method of that name may return instead.
pub(crate) fn is_test_case_instance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    module_class(db, env, "unittest", "TestCase").is_some_and(|test_case| {
        ty.is_subtype_of(
            db,
            env,
            Type::instance(db, env, test_case.default_specialization(db)),
        )
    })
}
