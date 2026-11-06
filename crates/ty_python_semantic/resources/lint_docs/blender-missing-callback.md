## What it does

Checks the classes that `register()` registers for a callback that Blender requires: `draw` on a
`Panel`, `Menu`, `Header`, `NodeSocket` or `Gizmo`, `setup` on a `GizmoGroup`, and `poll`,
`iterator` and `generate` on a `KeyingSetInfo`. The callback may come from any class of the add-on
or mixin the class derives from.

## Why is this bad?

Blender refuses to register the class, so `register()` raises and the add-on fails to enable.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.types import Panel

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class MY_PT_panel(Panel):  # error: [blender-missing-callback]
    bl_label = "My panel"
    bl_space_type = "VIEW_3D"
    bl_region_type = "UI"


def register():
    bpy.utils.register_class(MY_PT_panel)


def unregister():
    bpy.utils.unregister_class(MY_PT_panel)
```

Define `def draw(self, context)` on the panel.

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

class Panel(bpy_struct):
    bl_label: str
    bl_space_type: str
    bl_region_type: str
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
def unregister_class(cls: type) -> None: ...
```
