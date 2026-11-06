## What it does

Checks the `bl_idname` of the add-on's `AddonPreferences` classes. Blender finds an add-on's
preferences by the add-on's module name, so `bl_idname` must be the root package's name: `__package__`
in the root `__init__.py` or a direct submodule, `__name__` in the root `__init__.py`, or a legacy
add-on's folder name as a string. Reported are:

- a `bl_idname` that names something else, such as `__name__` in a submodule or another string;
- a preferences class without a `bl_idname`;
- a second registered preferences class with the add-on's `bl_idname`.

A `bl_idname` the checker cannot read (a parameter, a call) is not checked. The second class is only
reported when the checker can follow `register()` to its end, since that needs the order.

## Why is this bad?

Such a class registers without an error, but it is never the add-on's preferences:
`preferences.addons[__package__].preferences` is `None`, and the preferences are not drawn. A second
class with the add-on's `bl_idname` silently replaces the first, and unregistering the first then
fails.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.types import AddonPreferences

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class MyPreferences(AddonPreferences):
    bl_idname = "my_preferences"  # error: [blender-preferences-idname]


def register():
    bpy.utils.register_class(MyPreferences)


def unregister():
    bpy.utils.unregister_class(MyPreferences)
```

Use `bl_idname = __package__` (or `__name__` in `__init__.py`) instead.

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

class AddonPreferences(bpy_struct):
    bl_idname: str
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
def unregister_class(cls: type) -> None: ...
```
