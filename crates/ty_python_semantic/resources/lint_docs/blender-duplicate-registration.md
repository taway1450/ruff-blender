## What it does

Checks for a class that `register()` registers while it is still registered, and for two classes of
one kind (operators and macros; panels; menus; headers; UI lists) that register under the same
idname. A panel, menu, header or UI list without a `bl_idname` registers under its class name, and a
subclass that sets none inherits its parent's.

Registrations in different branches of an `if`, and registrations that Blender runs later (timers,
handlers), are not compared.

## Why is this bad?

Registering a class twice raises `ValueError`, so the add-on fails to enable. A second class with
the same idname silently replaces the first: the first operator or panel is gone, and unregistering
it later fails.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.types import Operator

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class SimpleOperator(Operator):
    bl_idname = "object.simple"


def register():
    bpy.utils.register_class(SimpleOperator)
    bpy.utils.register_class(SimpleOperator)  # error: [blender-duplicate-registration]


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
