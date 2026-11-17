## What it does

Checks the module-level reads of a member of `bpy.context` in a preset, a Python file in a folder
named `presets` (`bpy.context.camera` in `presets/camera/1_inch.py`). `scene`, `view_layer`,
`tool_settings` and `collection` are reported by `blender-assumed-context-member` instead, and a
read in a function of the file is not checked.

The read is typed without `None`. A read that the code only tests for truth or `None` is not
reported.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

Blender runs a preset with the context of the panel or menu that applies it
(`script.execute_preset`): a camera preset applied from the camera tab finds that camera in
`bpy.context.camera`. Nothing in the file says which panel runs it, and a preset run from a script
or another panel can find the member `None`.

## Examples

`presets/camera/small_sensor.py`:

```py
import bpy

bpy.context.camera.sensor_width = 13.2  # error: [blender-assumed-preset-context]
```

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import types as types
from .types import Context

context: Context
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...

class Camera(bpy_struct):
    sensor_width: float

class Context(bpy_struct):
    camera: Camera | None
```
