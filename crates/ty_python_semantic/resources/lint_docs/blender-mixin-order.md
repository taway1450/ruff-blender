## What it does

Checks the bases of a class deriving from a Blender class for a mixin listed after the Blender base
that defines a method Blender's own Python base class defines too: `Operator.as_keywords`,
`Menu.append`, `prepend` and `remove` (also on `Panel`, `Header` and `UIList`), `Menu.draw_preset`,
`Node.poll`, and a few more.

The callbacks Blender looks up when it registers the class (`execute`, `draw`, `Operator.poll`, ...)
are not affected: Blender finds them anywhere on the class, so a mixin's `poll` is used in either
order.

## Why is this bad?

Python looks a method up along the class's method resolution order, so Blender's base class wins
over a mixin listed after it, and the mixin's method is never called. Nothing reports it at runtime.

## Examples

```py
from bpy.types import Operator


class KeywordsMixin:
    def as_keywords(self, *, ignore=()):
        return {}


class MY_OT_run(Operator, KeywordsMixin):  # error: [blender-mixin-order]
    bl_idname = "my.run"
    bl_label = "Run"


class MY_OT_other(KeywordsMixin, Operator):  # ok: the mixin comes first
    bl_idname = "my.other"
    bl_label = "Other"
```

List the mixin before the Blender base.

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

class Operator(bpy_struct):
    bl_idname: str
    bl_label: str
```
