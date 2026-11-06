## What it does

Checks the order in which `register()` registers classes and adds properties. A class with a
`PointerProperty` or `CollectionProperty` to a `PropertyGroup` of the add-on, and a property
assigned to a Blender class with such a `type=`, need that `PropertyGroup` registered first. A
subpanel (`bl_parent_id`) needs its parent panel registered first.

The check follows `register()` through the functions it calls, the modules it imports, loops over
class lists and `bpy.utils.register_classes_factory`. Registrations that Blender runs later (timers,
handlers) have no known order and are not checked.

## Why is this bad?

Blender refuses the registration: a pointer to a `PropertyGroup` that is not registered yet raises
`ValueError`, and a subpanel before its parent raises `RuntimeError`, so the add-on fails to enable.
Unregistering in the wrong order works, so it is not reported.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.props import PointerProperty
from bpy.types import PropertyGroup

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class Leaf(PropertyGroup): ...


class Settings(PropertyGroup):
    leaf: PointerProperty(type=Leaf)


def register():
    bpy.utils.register_class(Settings)  # error: [blender-register-order]
    bpy.utils.register_class(Leaf)


def unregister():
    bpy.utils.unregister_class(Leaf)
    bpy.utils.unregister_class(Settings)
```

Register `Leaf` before `Settings` instead.

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

def PointerProperty(type: type, **kwargs: Any) -> _PropertyDeferred: ...
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...
class PropertyGroup(bpy_struct): ...
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
def unregister_class(cls: type) -> None: ...
```
