## What it does

Checks the reads of a member of `bpy.types.Context` (`context.object`, `context.screen`, ...) in a
callback of an operator whose `bl_options` has `'INTERNAL'`, its own or a base class's. The
operator's `poll` is left out, and so is a member its `poll` checked, which
`blender-narrowed-by-poll` reports.

The read is typed without `None`. A read that the code only tests for truth or `None`
(`if context.object is None:`) is not reported.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

`'INTERNAL'` hides an operator from the operator search. In ordinary use only the button, menu
entry or key of its own add-on calls it, from the editor and with the data it needs, so the code
reads them without a check. Nothing stops a script or a user key map from calling it anywhere
else, where the member can be `None`.

## Examples

```py
from bpy.types import Operator


class MY_OT_show(Operator):
    bl_idname = "my.show"
    bl_label = "Show"
    bl_options = {'INTERNAL'}

    def execute(self, context):
        print(context.object.name)  # error: [blender-assumed-internal-operator]
        return {'FINISHED'}
```

Check the member, or give the operator a `poll` that checks it.

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
class bpy_struct: ...

class Object(bpy_struct):
    name: str

class Context(bpy_struct):
    object: Object | None

class Operator(bpy_struct):
    bl_idname: str
    bl_label: str
    bl_options: set[str]
    @classmethod
    def poll(cls, context: Context) -> bool: ...
    def execute(self, context: Context) -> set[str]: ...
```
