## What it does

Checks the reads of a property that an add-on Blender enables by default registers, such as
`scene.cycles`, which Cycles adds to every scene. The read is typed by that add-on's registration
when the add-on's own code, a declared dependency (`[tool.blender] uses = ["cycles"]` in
`blender_manifest.toml`) and the startup packages do not give the attribute. The add-on is found
the way the file imports it, so its source must be on a search path.

The default add-ons are those of Blender's factory settings: `cycles`, `io_anim_bvh`,
`io_curve_svg`, `io_mesh_uv_layout`, `io_scene_fbx`, `io_scene_gltf2`, `pose_library` and
`bl_pkg`.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

A user can disable a default add-on. Its properties are then gone, and reading one raises
`AttributeError`. An add-on that needs the other add-on can declare it in its manifest, which
silences the rule, or check first (`hasattr(scene, "cycles")`).

## Examples

`my_addon/__init__.py`:

```py
from bpy.types import Scene

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


def samples(scene: Scene) -> int:
    return scene.cycles.samples  # error: [blender-assumed-default-addon]


def register(): ...
def unregister(): ...
```

`/.venv/<path-to-site-packages>/cycles/__init__.py`:

```py
import bpy
from bpy.props import IntProperty, PointerProperty
from bpy.types import PropertyGroup

bl_info = {"name": "Cycles", "blender": (4, 2, 0)}


class CyclesRenderSettings(PropertyGroup):
    samples: IntProperty()


def register():
    bpy.utils.register_class(CyclesRenderSettings)
    bpy.types.Scene.cycles = PointerProperty(type=CyclesRenderSettings)
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

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...
class ID(bpy_struct): ...
class Scene(ID): ...
class PropertyGroup(bpy_struct): ...
```

`/.venv/<path-to-site-packages>/bpy/props.pyi`:

```pyi
from typing import Any

class _PropertyDeferred: ...

def IntProperty(**kwargs: Any) -> _PropertyDeferred: ...
def PointerProperty(**kwargs: Any) -> _PropertyDeferred: ...
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
```
