## What it does

Checks for a property declaration assigned in the body of a class with a Blender base class (`value =
IntProperty()`), instead of annotated (`value: IntProperty()`).

## Why is this bad?

Before Blender 2.80, properties were declared as class attributes. Since then Blender only reads a class's
annotations: an assigned declaration stays a plain class attribute, no property is created, and Blender prints no
warning. Reading `self.value` then gives the declaration object, not the property's value.

Classes without a Blender base are not reported: a plain class may hold declarations on purpose, for example to
register them later with `setattr(bpy.types.Scene, name, Holder.value)`.

## Examples

```py
from bpy.props import IntProperty
from bpy.types import PropertyGroup

class Settings(PropertyGroup):
    value = IntProperty()  # error: [blender-legacy-property]
    count: IntProperty()  # what Blender reads
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
class PropertyGroup(bpy_struct): ...
```
