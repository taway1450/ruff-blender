## What it does

Checks the `bl_idname` of the classes the add-on registers. An operator or macro idname is
`category.name`: lower-case letters, digits and `_`, with exactly one `.` that is neither first nor
last, and at most 60 characters. A panel, menu, header or UI list idname is an upper-case prefix,
`_PT_`, `_MT_`, `_HT_` or `_UL_`, then a suffix (`MYADDON_PT_settings`); without a `bl_idname`, the
class name is the idname.

An idname that is not a string literal is not checked.

## Why is this bad?

Blender refuses an invalid operator idname, and an operator without one, with `RuntimeError`, so the
add-on fails to enable. For panels, menus, headers and UI lists it prints a warning, and code that
looks the class up by the conventional name does not find it.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.types import Operator

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class SimpleOperator(Operator):
    bl_idname = "OBJECT_OT_simple"  # error: [blender-invalid-idname]


def register():
    bpy.utils.register_class(SimpleOperator)


def unregister():
    bpy.utils.unregister_class(SimpleOperator)
```

Use `bl_idname = "object.simple"` instead.

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

class Operator(bpy_struct):
    bl_idname: str
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
def unregister_class(cls: type) -> None: ...
```
