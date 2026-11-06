## What it does

Checks for a class of the add-on that `bpy.utils.register_class` accepts (an operator, panel, menu,
UI list, `PropertyGroup`, `AddonPreferences`, ...) but that `register()` never registers, in any
module of the add-on, imported or not.

A class that another class of the add-on derives from is a base and needs no registration, and a
mixin derives from no Blender class. When `register()` does something the checker cannot follow (a
class list built at run time, or a call it cannot resolve), nothing is reported.

## Why is this bad?

Blender only knows the classes that are registered: an unregistered operator is not in `bpy.ops`, an
unregistered panel is never drawn, and unregistered add-on preferences are not the add-on's
preferences. Often the class was left out of the class list.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.types import Operator

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class SimpleOperator(Operator):
    bl_idname = "object.simple"


class OtherOperator(Operator):  # error: [blender-unregistered-class]
    bl_idname = "object.other"


def register():
    bpy.utils.register_class(SimpleOperator)


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
