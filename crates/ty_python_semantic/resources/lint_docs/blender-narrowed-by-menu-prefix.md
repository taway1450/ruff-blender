## What it does

Checks the reads of `context.space_data` and `context.area` in a Menu's `draw` and `poll` when the
menu's idname (its `bl_idname`, or without one its class name) starts with an editor's prefix:
`CLIP_MT_view` belongs to the clip editor, whose header is `CLIP_HT_header`. The 19 header prefixes
of Blender's editors, `TIME` (the dope sheet) and `ASSETBROWSER` (the file browser) count.

`space_data` is typed as that editor's `Space` subclass (`SpaceClipEditor`), or `Space` for the top
bar and the status bar, and `area` without `None`. Another prefix, such as the add-on's own, gives
nothing.

The rule belongs to the category `blender-implied-narrowings`.

## Why is this bad?

A menu has no `bl_space_type`: Blender draws it in whatever editor `layout.menu(...)`,
`wm.call_menu` or a key map item opens it from. Blender's own menus follow the naming convention
and menu search relies on it, but a menu drawn from another editor (a submenu shared between
editors, a user's key map entry) gets that editor's space.

## Examples

```py
from bpy.types import Menu


class CLIP_MT_my_tools(Menu):
    def draw(self, context):
        clip = context.space_data.clip  # error: [blender-narrowed-by-menu-prefix]


class MY_MT_tools(Menu):
    def draw(self, context):
        clip = context.space_data.clip  # error: [missing-attribute]
```

Check `context.space_data.type` (or `context.area.type`) in the menu's `poll`.

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
    type: Literal["EMPTY", "CLIP_EDITOR", "VIEW_3D"]

class MovieClip(bpy_struct): ...

class SpaceClipEditor(Space):
    type: Literal["CLIP_EDITOR"]
    clip: MovieClip | None

class Area(bpy_struct): ...

class Context(bpy_struct):
    area: Area | None
    space_data: Space | None

class Menu(bpy_struct):
    @classmethod
    def poll(cls, context: Context) -> bool: ...
    def draw(self, context: Context) -> None: ...
```
