## What it does

Checks for a class with two Blender bases where neither derives from the other, such as an
`Operator` that is also a `Panel`.

ty's own checks of the conflicts between the two bases (`invalid-attribute-override`,
`invalid-method-override`) are left out on such a class, since they all come from this one mistake.

## Why is this bad?

Blender registers the class as its first Blender base only, so the class is never what it was
meant to be: an `Operator` and `Panel` is registered as an operator, and never drawn as a panel.

## Examples

```py
from bpy.types import Operator, Panel


class MY_OT_run(Operator, Panel):  # error: [blender-invalid-base]
    bl_idname = "my.run"
    bl_label = "Run"
```

Split it into two classes, one per Blender base.

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import types as types
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...

class Operator(bpy_struct):
    bl_idname: str
    bl_label: str

class Panel(bpy_struct):
    bl_idname: str
    bl_label: str
```
