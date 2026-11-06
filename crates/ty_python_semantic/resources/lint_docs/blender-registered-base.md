## What it does

Checks for a class that `register()` registers while it also registers a class the first one
derives from, in either order. A `PropertyGroup` deriving from another registered `PropertyGroup` is
fine, and registrations in different branches of an `if` are not compared.

## Why is this bad?

When Blender registers a subclass of a registered class, it detaches the base from its Python
class. The base stays registered but its callbacks are no longer called: an operator returns
`{'FINISHED'}` without running `execute`, a panel or menu draws nothing, a node can no longer be
added. Blender only logs that it is "unable to get Python class". Registered the other way round,
both work, but only because of the order; Blender's `--debug-python` check warns about either order.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.types import Operator

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class MY_OT_base(Operator):
    bl_idname = "my.base"


class MY_OT_child(MY_OT_base):  # error: [blender-registered-base]
    bl_idname = "my.child"


def register():
    bpy.utils.register_class(MY_OT_base)
    bpy.utils.register_class(MY_OT_child)


def unregister():
    bpy.utils.unregister_class(MY_OT_child)
    bpy.utils.unregister_class(MY_OT_base)
```

Share the code through a base class that is not registered, or through a mixin.

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
