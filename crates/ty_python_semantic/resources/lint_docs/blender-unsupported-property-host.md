## What it does

Checks for property annotations (`value: IntProperty()`) in the body of a Blender class whose annotations Blender
ignores: a `Panel`, `Menu`, `Header`, `KeyingSetInfo`, `RenderEngine`, `FileHandler`, `AssetShelf` and the
like. Only `PropertyGroup`, `Operator`, `Macro`, `AddonPreferences`, `UIList`, `Node`, `NodeSocket`, `NodeTree`,
`NodeTreeInterfaceSocket`, `KeyConfigPreferences` and `Gizmo` turn their annotations into properties.

## Why is this bad?

Blender registers such a class without an error, but creates no property from the annotation, so reading
`self.value` raises `AttributeError`. To store a setting for a panel, declare it on a `PropertyGroup` (or as a
Registered property of the scene) and draw that.

A plain class that is used as a mixin is not reported: its annotations become properties of the hosts that
inherit them.

## Examples

```py
from bpy.props import IntProperty
from bpy.types import Panel

class VIEW3D_PT_settings(Panel):
    bl_label = "Settings"
    bl_space_type = "VIEW_3D"
    bl_region_type = "UI"

    value: IntProperty()  # error: [blender-unsupported-property-host]
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
class Panel(bpy_struct): ...
```
