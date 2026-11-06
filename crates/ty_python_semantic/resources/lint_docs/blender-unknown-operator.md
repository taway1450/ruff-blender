## What it does

Checks the operator idnames given as strings to Blender's functions that take one
(`layout.operator(...)`, `operator_menu_hold`, `operator_menu_enum`, `operator_enum`,
`keymap_items.new(...)`, `operator_properties_last`, `Gizmo.target_set_operator`,
`WorkSpaceTool.operator_properties`, `Macro.define`) and in the `bl_keymap` of a tool, for an idname
that names neither an operator the add-on registers nor one of Blender's own. Both forms are looked
up as Blender does: `"object.select_all"` and `"OBJECT_OT_select_all"`.

Another add-on's operator counts as unknown. When `register()` does something the checker cannot
follow, or the file belongs to no add-on, nothing is reported: the operator may be registered where
the checker cannot see it.

## Why is this bad?

Blender only prints a warning for an unknown operator: the button or menu entry is missing,
`layout.operator(...)` returns `None` (so setting one of its properties raises `AttributeError`),
and a key-map item does nothing. Often the idname has a typo, or the operator is not registered.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.types import Operator, UILayout

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class MY_OT_run(Operator):
    bl_idname = "my.run"


def draw(layout: UILayout) -> None:
    layout.operator("my.run")
    layout.operator("my.rnu")  # error: [blender-unknown-operator]


def register():
    bpy.utils.register_class(MY_OT_run)


def unregister():
    bpy.utils.unregister_class(MY_OT_run)
```

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import types as types, utils as utils
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...
class OperatorProperties(bpy_struct): ...

class Operator(bpy_struct):
    bl_idname: str

class UILayout(bpy_struct):
    def operator(self, operator: str) -> OperatorProperties | None: ...
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
def unregister_class(cls: type) -> None: ...
```
