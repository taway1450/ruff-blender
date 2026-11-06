## What it does

Checks for a function of the add-on that registers classes (it calls `bpy.utils.register_class`, or
the register function of `bpy.utils.register_classes_factory`) but that `register()` never calls,
directly or through other functions. A function that `register()` hands to Blender, such as a timer,
counts as called.

When `register()` does something the checker cannot follow, nothing is reported. The classes such a
function registers are not also reported as `blender-unregistered-class`.

## Why is this bad?

Blender only runs `register()`, so the classes the function registers are never registered. Often a
submodule's `register()` is not called from the add-on's.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.types import Operator

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class SimpleOperator(Operator):
    bl_idname = "object.simple"


def register_simple():  # error: [blender-unreachable-registration]
    bpy.utils.register_class(SimpleOperator)


def register():
    pass


def unregister():
    pass
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

class Operator(bpy_struct):
    bl_idname: str
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
def unregister_class(cls: type) -> None: ...
```
