## What it does

Checks the reads of `context.space_data` in a Panel that the same add-on opens as a popover, with
`layout.popover(...)` or `wm.call_panel`: its `space_data` is typed as the `Space` subclass of the
panel's `bl_space_type`, as in any panel, and reported. A panel the add-on never opens as a popover
gets the same type without a report.

In a panel of the `'HEADER'` region type, which Blender draws only as a popover, it also checks the
reads of a pointer that every function opening the panel checked before the call, by its own control
flow (`if context.space_data.node_tree: layout.popover(...)`, an early `return`): the read is typed
without `None`, or as the subclass a Type tag test names, in the panel's `draw` and the other
callbacks a poll would gate, and reported. The checks are read as in a `poll()`
(`blender-narrowed-by-poll`).

The rule belongs to the category `blender-implied-narrowings`.

## Why is this bad?

Blender draws a panel in an editor of its `bl_space_type`, but a popover is drawn with the area of
whatever editor opened it, whatever `bl_space_type` says. Blender's own popover-only panels often
give a dummy `bl_space_type`. The panel's `space_data` is that editor's only when every place that
opens it is in that editor. Likewise, the popover is drawn with the context of the editor that
opened it, so what the opener checked still holds, unless the user changes it while the popover is
open.

## Examples

```py
from bpy.types import Panel


class MY_PT_settings(Panel):
    bl_space_type = "VIEW_3D"

    def draw(self, context):
        lens = context.space_data.lens  # error: [blender-narrowed-by-popover]


class MY_PT_tools(Panel):
    bl_space_type = "VIEW_3D"

    def draw(self, context):
        lens = context.space_data.lens
        self.layout.popover("MY_PT_settings")
```

Check `context.space_data.type` before reading what only one editor's space has.

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
from typing import Literal

class bpy_struct: ...

class Space(bpy_struct):
    type: Literal["EMPTY", "VIEW_3D"]

class SpaceView3D(Space):
    type: Literal["VIEW_3D"]
    lens: float

class Context(bpy_struct):
    space_data: Space | None

class UILayout(bpy_struct):
    def popover(self, panel: str) -> None: ...

class Panel(bpy_struct):
    layout: UILayout | None
    bl_space_type: str
    def draw(self, context: Context) -> None: ...
```
