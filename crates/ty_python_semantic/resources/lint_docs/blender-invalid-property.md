## What it does

Checks the `bpy.props` calls of a Blender add-on (`IntProperty(...)`, `EnumProperty(...)`, ...) for arguments
that Blender rejects when it registers the property:

- an enum default that is not one of the static items, by identifier or by number;
- with `options={"ENUM_FLAG"}`, a default that is not a set of item identifiers, or more than 32 items;
- a set default without `ENUM_FLAG`;
- a `str` default with function `items=`;
- a vector default whose length or nesting differs from `size=` (3 when it is not given);
- a `size=` outside 1 to 32, or with more than three dimensions;
- `set=` without `get=`;
- a property name that starts with `_`;
- a pointer to a data-block (`PointerProperty(type=Object)`), or to a group that holds one, on a class that
  cannot hold one, such as `AddonPreferences` (on an operator or a macro, this is `blender-invalid-operator-property`).

## Why is this bad?

Blender reads a declaration only when it registers the property: when `register_class` registers the class that
annotates it, or at the assignment `bpy.types.Scene.x = IntProperty(...)`. A declaration Blender rejects raises a
`ValueError` there, so the add-on fails to enable.

A call is checked where it is written, so a shared declaration (`COMMON = IntProperty(...)`) is reported once, at
its definition. Each problem is reported on the offending argument, or on the name for a name that starts with `_`.
Arguments the checker cannot read (a name it cannot follow, a `**mapping`) are not reported. Annotations on a class
whose annotations Blender ignores (a `Panel`) are not checked, as Blender never evaluates them.

Arguments that the stubs already reject, such as `IntProperty(default=1.5)` or a `type=` of the wrong kind, are
left to `invalid-argument-type`. `min` and `max` are not checked: Blender accepts any of them.

## Examples

```py
from bpy.props import EnumProperty, FloatVectorProperty, IntProperty
from bpy.types import PropertyGroup

class Settings(PropertyGroup):
    mode: EnumProperty(items=[("A", "A", ""), ("B", "B", "")], default="C")  # error: [blender-invalid-property]
    offset: FloatVectorProperty(size=3, default=(1.0, 2.0))  # error: [blender-invalid-property]
    count: IntProperty(set=lambda self, value: None)  # error: [blender-invalid-property]
```

The examples use these minimal Blender stubs:

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

def EnumProperty(**kwargs: Any) -> _PropertyDeferred: ...
def FloatVectorProperty(**kwargs: Any) -> _PropertyDeferred: ...
def IntProperty(**kwargs: Any) -> _PropertyDeferred: ...
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...
class PropertyGroup(bpy_struct): ...
```
