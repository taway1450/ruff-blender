## What it does

Checks for a property declaration assigned to a Blender class that does not accept Registered properties
(`bpy.types.Area.value = IntProperty()`).

Every ID type (`Scene`, `Object`, `Material`, ...) accepts them, and so do `PropertyGroup`, nodes and sockets,
bones, `BoneCollection`, `ViewLayer`, strips, `TimelineMarker`, `View3DShading` and a few structs that hold
operator, gizmo and modifier settings. Other classes, such as `Area`, `Modifier`, `Window`, `Preferences`,
`Context`, `Operator` and `Panel`, do not.

## Why is this bad?

Blender turns an assignment to an accepting class into a property of every instance. On any other class the
assignment silently stores the declaration as a plain class attribute: no property is created, no error is raised,
and `area.value` raises `AttributeError` on an instance.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.props import IntProperty

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}

def register():
    bpy.types.Scene.value = IntProperty()
    bpy.types.Area.value = IntProperty()  # error: [blender-unsupported-property-target]

def unregister():
    del bpy.types.Scene.value
```

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import props as props, types as types
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
class Area(bpy_struct): ...
```
