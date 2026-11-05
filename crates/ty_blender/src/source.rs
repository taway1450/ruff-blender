//! What a Python file says about Blender without being type-checked: its top-level `bl_info`.

use ruff_db::Db;
use ruff_db::files::File;
use ruff_db::source::source_text;
use ruff_python_ast::{self as ast, PySourceType};
use ruff_python_parser::parse_unchecked_source;
use ruff_text_size::{Ranged, TextRange};

/// A module-level `bl_info = ...` binding.
#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize)]
pub struct BlInfo {
    /// The target name `bl_info`.
    pub target: TextRange,
    /// The assigned value, when it is a dict display: each entry's string key, the key's range and the
    /// entry's range. `None` when the value is not a dict display.
    pub dict: Option<BlInfoDict>,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize)]
pub struct BlInfoDict {
    pub range: TextRange,
    pub entries: Vec<BlInfoEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize)]
pub struct BlInfoEntry {
    /// The key, when it is a string literal.
    pub key: Option<String>,
    pub range: TextRange,
    /// Whether the value is a tuple display of exactly three int literals (what `"blender"` needs).
    pub is_int_triple: bool,
}

/// The module-level `bl_info` binding of `file`, if it has one. Only top-level statements count.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn bl_info(db: &dyn Db, file: File) -> Option<BlInfo> {
    let source = source_text(db, file);
    let parsed = parse_unchecked_source(source.as_str(), PySourceType::Python);
    parsed.syntax().body.iter().rev().find_map(bl_info_binding)
}

fn bl_info_binding(statement: &ast::Stmt) -> Option<BlInfo> {
    let (target, value) = match statement {
        ast::Stmt::Assign(assign) => match assign.targets.as_slice() {
            [target] => (target, Some(&*assign.value)),
            _ => return None,
        },
        ast::Stmt::AnnAssign(assign) => (&*assign.target, assign.value.as_deref()),
        _ => return None,
    };
    let ast::Expr::Name(name) = target else {
        return None;
    };
    if name.id.as_str() != "bl_info" {
        return None;
    }
    let dict = value
        .and_then(|value| value.as_dict_expr())
        .map(|dict| BlInfoDict {
            range: dict.range(),
            entries: dict
                .items
                .iter()
                .map(|item| BlInfoEntry {
                    key: item
                        .key
                        .as_ref()
                        .and_then(|key| key.as_string_literal_expr())
                        .map(|key| key.value.to_str().to_string()),
                    range: TextRange::new(
                        item.key.as_ref().map_or(item.value.start(), Ranged::start),
                        item.value.end(),
                    ),
                    is_int_triple: is_int_triple(&item.value),
                })
                .collect(),
        });
    Some(BlInfo {
        target: name.range(),
        dict,
    })
}

fn is_int_triple(value: &ast::Expr) -> bool {
    let ast::Expr::Tuple(tuple) = value else {
        return false;
    };
    tuple.elts.len() == 3
        && tuple.elts.iter().all(|element| {
            matches!(
                element,
                ast::Expr::NumberLiteral(ast::ExprNumberLiteral {
                    value: ast::Number::Int(_),
                    ..
                })
            )
        })
}
