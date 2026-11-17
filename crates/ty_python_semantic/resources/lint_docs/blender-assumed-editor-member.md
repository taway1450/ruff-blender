## What it does

Checks the reads of `area`, `region` and `space_data` on a `bpy.types.Context` in code Blender
calls while it draws an editor, where the class names no editor of its own: the `poll` of a
`Panel`, `Menu` or `AssetShelf`, a tool's `draw_settings`, and `RenderEngine.view_update` /
`view_draw`. A `Panel` with a `bl_space_type` and a menu whose idname prefix names an editor are
narrowed by the context member and `blender-narrowed-by-menu-prefix` rules instead.

The read is typed without `None`. A read that the code only tests for truth or `None`
(`if context.region is None:`) is not reported.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

Blender polls a panel or a menu while drawing a region of an editor, draws a tool's settings in
the tool header or the tool settings of an editor, and calls `view_update` and `view_draw` for the
3D viewport that shows the render, so these members are set there in ordinary use. A script,
`wm.call_menu` or `wm.call_panel` can poll a menu or a panel with no editor, where they are `None`.

## Examples

```py
from bpy.types import Menu


class MY_MT_menu(Menu):
    bl_label = "Menu"

    @classmethod
    def poll(cls, context):
        return context.space_data.type == 'VIEW_3D'  # error: [blender-assumed-editor-member]
```

Check the member before reading through it.

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

class Space(bpy_struct):
    type: str

class Context(bpy_struct):
    space_data: Space | None

class Menu(bpy_struct):
    bl_label: str
    @classmethod
    def poll(cls, context: Context) -> bool: ...
    def draw(self, context: Context) -> None: ...
```
