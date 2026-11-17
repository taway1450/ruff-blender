## What it does

Checks the uses of a class or a Registered property that `register()` registers only on a branch:
in an arm of an `if` or `match`, in an `except` handler, in a function Blender calls later (a timer
or a handler), or from a list extended at run time. The uses are an operator call
`bpy.ops.<category>.<name>`, a read of the Registered property and a read of the add-on's
`preferences`. The class or property is taken as registered.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

The class or property exists only when the condition held. Usually the code that uses it runs under
the same condition, but the typechecker cannot see that: on a Blender version or a build where the
branch did not run, the operator call fails and the property read raises `AttributeError`.

## Examples

`my_addon/__init__.py`:

```py
import os

import bpy
from bpy.types import Operator

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class MY_OT_new(Operator):
    bl_idname = "my.new"


class MY_OT_run(Operator):
    bl_idname = "my.run"

    def execute(self, context):
        bpy.ops.my.new()  # error: [blender-assumed-registration]
        return {"FINISHED"}


def register():
    bpy.utils.register_class(MY_OT_run)
    if os.environ.get("MY_ADDON_NEW") is not None:
        bpy.utils.register_class(MY_OT_new)


def unregister():
    if MY_OT_new.is_registered:
        bpy.utils.unregister_class(MY_OT_new)
    bpy.utils.unregister_class(MY_OT_run)
```

Check the same condition where the class is used.

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import ops as ops, types as types, utils as utils
```

`/.venv/<path-to-site-packages>/bpy/ops/__init__.pyi`:

```pyi
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct:
    is_registered: bool

class Context(bpy_struct): ...

class Operator(bpy_struct):
    bl_idname: str
    def execute(self, context: Context) -> set[str]: ...
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
def unregister_class(cls: type) -> None: ...
```
