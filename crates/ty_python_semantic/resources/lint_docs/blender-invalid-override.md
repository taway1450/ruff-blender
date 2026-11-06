## What it does

Checks the methods of a class deriving from a Blender class that `register_class` accepts
(`Operator`, `Panel`, `UIList`, `Node`, ...) that Blender calls: `execute`, `invoke`, `poll`,
`draw`, `draw_item` and the other callbacks of that class. Blender refuses such a method when:

- it has the wrong number of positional parameters, counting `self` or `cls` and parameters with
  defaults, but not `*args`, keyword-only parameters or `**kwargs`. `UIList.draw_item` accepts 7 to
  10;
- it is a plain method where Blender needs a classmethod (`poll`, `description`, ...), or a
  classmethod where Blender needs a plain method (`execute`, `draw`, ...). A `staticmethod` passes
  where a plain method is needed.

Every class deriving from a Blender class is checked, registered or not, since a registered
subclass inherits the method.

Blender passes a callback's arguments by position, as many as it has parameters, so a renamed
parameter (`def execute(self, ctx)`) or a short `draw_item` is fine. ty's `invalid-method-override`
accepts them too, and leaves a method that this rule reports to it.

## Why is this bad?

Blender raises `ValueError` when it registers the class (`expected Operator, MY_OT_run class
"invoke" function to have 3 args, found 2`), so the add-on fails to enable.

## Examples

```py
from bpy.types import Operator


class MY_OT_run(Operator):
    bl_idname = "my.run"
    bl_label = "Run"

    def execute(self, ctx):  # ok: Blender passes the context by position
        return {"FINISHED"}

    def invoke(self, context):  # error: [blender-invalid-override]
        return self.execute(context)
```

Blender calls `invoke` with the context and the event: use `def invoke(self, context, event)`.

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
class Context(bpy_struct): ...
class Event(bpy_struct): ...

class Operator(bpy_struct):
    bl_idname: str
    bl_label: str
    def execute(self, context: Context) -> set[str]: ...
    def invoke(self, context: Context, event: Event) -> set[str]: ...
```
