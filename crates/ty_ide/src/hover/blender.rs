//! Hover on a Blender operator idname string (`layout.operator("my_addon.run")`): the operator's
//! label, description and properties.

use ruff_db::files::FileRange;
use ruff_text_size::TextSize;
use ty_python_core::ProgramFile;

use super::{Hover, HoverContent};
use crate::docstring::Docstring;
use crate::{Db, RangedValue};

/// The hover for the operator idname string at `offset`, when there is one.
pub(super) fn idname_hover<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    offset: TextSize,
) -> Option<RangedValue<Hover<'db>>> {
    let (range, docs) = ty_python_semantic::types::blender_idname_hover(db, file, offset)?;
    Some(RangedValue {
        range: FileRange::new(file.file(db), range),
        value: Hover {
            program_file: file,
            contents: vec![HoverContent::Docstring(Docstring::new(docs))],
        },
    })
}
