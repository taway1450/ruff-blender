## What it does

Compares `unregister()` with `register()`. It reports a class that `register()` registers and
`unregister()` never unregisters, a property `register()` adds to a Blender class and `unregister()`
never deletes, a handler or menu function `register()` appends and `unregister()` never removes, and
a class `unregister()` unregisters that `register()` never registers.

When either function does something the checker cannot follow, what depends on it is not reported.
Deleting or removing something that was never added is not reported either.

## Why is this bad?

Whatever `unregister()` leaves behind stays in Blender after the add-on is disabled: its operators
and panels, its properties, and its handlers, which keep running. Enabling the add-on again then
registers the classes twice. Unregistering a class that is not registered raises `RuntimeError`, so
disabling the add-on fails.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.props import IntProperty
from bpy.types import Operator

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class SimpleOperator(Operator):
    bl_idname = "object.simple"


def register():
    bpy.utils.register_class(SimpleOperator)
    bpy.types.Scene.count = IntProperty()  # error: [blender-unregister-mismatch]


def unregister():
    bpy.utils.unregister_class(SimpleOperator)
```

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import props as props, types as types, utils as utils
```

`/.venv/<path-to-site-packages>/bpy/props.pyi`:

```pyi
from typing import Any

class _PropertyDeferred: ...

def IntProperty(**kwargs: Any) -> _PropertyDeferred: ...
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...
class ID(bpy_struct): ...
class Scene(ID): ...

class Operator(bpy_struct):
    bl_idname: str
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
def unregister_class(cls: type) -> None: ...
```
