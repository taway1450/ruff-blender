## What it does

Checks the values passed where Blender takes an enum item, for a value typed `str` (or `set[str]`
for an enum flag set): an argument of a function, method or operator of Blender's API, or the value
assigned to an enum property of a Blender object, whose stubs spell the enum as a `Literal`.

The value is accepted as the enum. A string literal is precise: one that is not an item is still
an error, alone (`mode="OBJCT"`) or in a set display (`{"VERT", "EDGEE"}`), and an add-on's own
`Literal` parameter keeps ty's check.

The rule belongs to the category `blender-imprecise-annotations`.

## Why is this bad?

Blender checks the name itself and raises for one that is not an item (`TypeError: enum "OBJCT"
not found in (...)`). A value computed at runtime hides a wrong name from the typechecker.

## Examples

```py
from bpy.types import Light, LightCollection


def add_light(lights: LightCollection, light_type: str) -> None:
    lights.new("Light", light_type)  # error: [blender-imprecise-enum-value]
    lights.new("Light", "SUN")


def set_type(light: Light, light_type: str) -> None:
    light.type = light_type  # error: [blender-imprecise-enum-value]
    light.type = "SUNN"  # error: [invalid-assignment]
```

Annotate the value with the enum's `Literal` type where it is computed.

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import types as types
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
from typing import Literal

class bpy_struct: ...

class Light(bpy_struct):
    type: Literal["POINT", "SUN", "SPOT", "AREA"]

class LightCollection(bpy_struct):
    def new(self, name: str, type: Literal["POINT", "SUN", "SPOT", "AREA"]) -> Light: ...
```
