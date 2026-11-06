## What it does

Checks the properties of an operator or a macro for one that Blender refuses: a pointer to a
data-block (`PointerProperty(type=Object)`), or a pointer or collection to a `PropertyGroup` that holds
one, at any depth. It is reported on the property's name, or on the operator's class name when the
property comes from a mixin that is not an operator.

## Why is this bad?

An operator's properties cannot point to data-blocks. Blender prints a `ValueError` when it registers
the operator, and registers it without that property **and every property after it**:
`bpy.ops.my_addon.run(after=1)` then raises `TypeError`, and the properties are missing from
`self.properties` and the operator's panel.

Store the name of the data-block instead (`StringProperty()`), and look it up in `bpy.data` when the
operator runs.

## Examples

```py
from bpy.props import IntProperty, PointerProperty
from bpy.types import Object, Operator

class MY_OT_run(Operator):
    bl_idname = "my.run"
    bl_label = "Run"

    before: IntProperty()
    target: PointerProperty(type=Object)  # error: [blender-invalid-operator-property]
    after: IntProperty()
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
def PointerProperty(*, type: type, **kwargs: Any) -> _PropertyDeferred: ...
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...
class ID(bpy_struct): ...
class Object(ID): ...

class Operator(bpy_struct):
    bl_idname: str
    bl_label: str
```
