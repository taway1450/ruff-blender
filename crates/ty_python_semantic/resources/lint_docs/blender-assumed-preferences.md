## What it does

Checks the reads of the add-on's own preferences, `preferences.addons[KEY].preferences`, that run
while its `AddonPreferences` class is not registered: in `register()` (or a function it calls)
before `register_class` of the class, and in `unregister()` after `unregister_class`. The read is
still typed as the add-on's preferences class. A read in a callback, or after the class is
registered, is not reported.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

Blender adds the add-on's entry before it calls `register()`, but its `preferences` is `None` until
the preferences class is registered, and `None` again once it is unregistered. Reading a setting
there raises `AttributeError`.

## Examples

`my_addon/__init__.py`:

```py
import bpy
from bpy.types import AddonPreferences

bl_info = {"name": "My add-on", "blender": (4, 2, 0)}


class MyPreferences(AddonPreferences):
    bl_idname = __package__


def register():
    prefs = bpy.context.preferences.addons[__package__].preferences  # error: [blender-assumed-preferences]
    bpy.utils.register_class(MyPreferences)


def unregister():
    bpy.utils.unregister_class(MyPreferences)
```

Register the preferences class first.

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import types as types, utils as utils
from .types import Context

context: Context
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...

class AddonPreferences(bpy_struct):
    bl_idname: str

class Addon(bpy_struct):
    preferences: AddonPreferences | None

class Addons(bpy_struct):
    def __getitem__(self, key: str) -> Addon: ...

class Preferences(bpy_struct):
    addons: Addons

class Context(bpy_struct):
    preferences: Preferences
```

`/.venv/<path-to-site-packages>/bpy/utils.pyi`:

```pyi
def register_class(cls: type) -> None: ...
def unregister_class(cls: type) -> None: ...
```
