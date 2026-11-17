## What it does

Checks the reads of a pointer that the class's `poll()` checked, in a callback the poll gates: a
Panel's `draw`, `draw_header` and `draw_header_preset`, a Menu's `draw`, an Operator's `invoke` and
`execute`, and a GizmoGroup's `setup`, `refresh`, `draw_prepare` and `invoke_prepare`.

The read is typed without `None`, and a Type tag test in the poll (`ob.type == 'MESH'`) narrows it
as the same test would in the callback. A mode test (`context.mode == 'EDIT_MESH'`) checks what
Blender's mode implies: the edit object, of the mode's object type, or a paint mode's tool
settings. A pointer Blender keeps in step with a checked one (`space_data.edit_tree` with
`space_data.node_tree`) is narrowed too, without a report. The poll's checks are read from its code: the conjuncts of
its `return`, attribute reads always evaluated while it computes the result (a poll that raises
counts as `False`), early `return False` guards, local aliases, `super().poll(context)`, an
inherited poll and helper functions taking the context. A check is about one path from the context
(`context.object.active_material`), so a pointer the poll did not check is still an error. A read
the callback only tests for truth or `None` is not reported.

The rule belongs to the category `blender-implied-narrowings`.

## Why is this bad?

Blender calls these callbacks only right after `poll()` returned `True`, with the same context, so
the read is safe while the callback runs that way. It is not when the callback is called another
way (`self.execute(context)` from `modal`, which Blender calls without a new poll) or when it
changes the context before the read, which the typechecker cannot see.

## Examples

```py
from bpy.types import Panel


class MY_PT_material(Panel):
    @classmethod
    def poll(cls, context):
        return context.object is not None

    def draw(self, context):
        self.layout.label(text=context.object.name)  # error: [blender-narrowed-by-poll]
        self.layout.label(text=context.material.name)  # error: [missing-union-attribute]
```

Check the pointer in the callback too, or read it once into a checked local.

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

class Material(bpy_struct):
    name: str

class Context(bpy_struct):
    object: Object | None
    material: Material | None

class UILayout(bpy_struct):
    def label(self, *, text: str = "") -> None: ...

class Panel(bpy_struct):
    layout: UILayout | None
    @classmethod
    def poll(cls, context: Context) -> bool: ...
    def draw(self, context: Context) -> None: ...
```
