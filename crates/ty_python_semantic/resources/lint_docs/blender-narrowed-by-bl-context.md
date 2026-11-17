## What it does

Checks the reads of a context member that a Panel's `bl_context` guarantees, where Blender enforces
it: in the panel's `poll`, in the callbacks the poll gates (`draw`, `draw_header`,
`draw_header_preset`) and in those of its sub-panels.

Blender checks a top-level panel's `bl_context` against its region's contexts before it polls the
panel, and draws a sub-panel only inside its parent. Two kinds of region enforce it:

- In the Properties editor's main region (`bl_space_type = "PROPERTIES"`), a panel is drawn only in
  its tab, and a tab is shown only while its data exists. The `object`, `physics`, `constraint`,
  `modifier`, `shaderfx` and `bone_constraint` tabs need an object, so `context.object` is typed
  without `None` there (of the Grease Pencil or armature type for the last two).
- In the 3D viewport's sidebar and toolbar (`bl_space_type = "VIEW_3D"`, `bl_region_type` `"UI"`
  or `"TOOLS"`), a panel whose `bl_context` is a mode (`"mesh_edit"`, `"posemode"`,
  `"sculpt_mode"`, ...) is drawn only in that mode, which gives what `context.mode == 'EDIT_MESH'`
  gives in a `poll`: the edit object of the mode's type, a paint mode's tool settings, the pose of
  an armature.

Not narrowed: the `data`, `material` and `particle` tabs, which also show pinned data without an
object; `"objectmode"`, which needs no object; the display-only contexts that start with a dot
(`".objectmode"`, `".mesh_edit"`, `".paint_common"`), groups of tool settings that the tool header
also draws as popovers; a mode in any other editor; and a panel the add-on opens as a popover, where
no context is checked.

The rule belongs to the category `blender-implied-narrowings`.

## Why is this bad?

The read is safe while Blender draws the panel where its `bl_context` says. It is not when the panel
is drawn another way, for example as a popover opened from another add-on, or when the code changes
the context before the read, which the typechecker cannot see.

## Examples

```py
from bpy.types import Panel


class MY_PT_object(Panel):
    bl_space_type = "PROPERTIES"
    bl_region_type = "WINDOW"
    bl_context = "object"

    def draw(self, context):
        self.layout.label(text=context.object.name)  # error: [blender-narrowed-by-bl-context]


class MY_PT_tools(Panel):
    bl_space_type = "VIEW_3D"
    bl_region_type = "UI"
    bl_context = ".objectmode"

    def draw(self, context):
        self.layout.label(text=context.object.name)  # error: [missing-union-attribute]
```

Check the pointer in the panel's `poll` or in the callback if the panel may be drawn elsewhere.

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

class UILayout(bpy_struct):
    def label(self, *, text: str = "") -> None: ...

class Panel(bpy_struct):
    layout: UILayout | None
    @classmethod
    def poll(cls, context: Context) -> bool: ...
    def draw(self, context: Context) -> None: ...
```
