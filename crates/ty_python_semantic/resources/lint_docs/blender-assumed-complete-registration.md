## What it does

Reports, once per add-on, where `register()` registers a value the typechecker cannot follow: a
class found at run time (`getattr`, `importlib.import_module`, an `auto_load` module), or a call it
cannot resolve. From there on every class and Registered property in the add-on's files counts as
registered, and the checks for classes that `register()` misses (`blender-unregistered-class`,
`blender-unreachable-registration`, ...) are off.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

A class or property the add-on defines but never registers is then typed as present, and nothing
reports it: silence here does not mean the add-on registers everything it uses.

## Examples

`my_addon/__init__.py`:

```py
import sys

import bpy
from bpy.types import Operator

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}

CLASS_NAMES = ("MY_OT_run",)


class MY_OT_run(Operator):
    bl_idname = "my.run"


def register():
    module = sys.modules[__name__]
    for name in CLASS_NAMES:
        bpy.utils.register_class(getattr(module, name))  # error: [blender-assumed-complete-registration]


def unregister():
    bpy.utils.unregister_class(MY_OT_run)
```

List the classes in `register()` (or in a tuple it registers) to have them checked.

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
