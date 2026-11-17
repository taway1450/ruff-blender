## What it does

Checks the reads of the `item` parameter of a `UIList.draw_item` override that leaves it
unannotated. The parameter is typed without `None`; a read that the method only tests for truth
or `None` (`if item is None: return`) is not reported, nor are the reads it narrows itself.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

Blender passes `None` as the item only for an empty slot of an array of ID pointers. A list of an
add-on's own collection never has one, so the read is safe there, but the stubs cannot tell which
list the class draws.

## Examples

```py
from bpy.types import UIList


class MY_UL_items(UIList):
    def draw_item(self, context, layout, data, item, icon, active_data, active_property):
        layout.prop(item, "name")  # error: [blender-assumed-list-item]
```

Check `item` for `None` first when the list may show empty slots.

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

class UILayout(bpy_struct):
    def prop(self, data: bpy_struct, property: str) -> None: ...

class UIList(bpy_struct):
    def draw_item(
        self,
        context: Context,
        layout: UILayout,
        data: bpy_struct,
        item: bpy_struct | None,
        icon: int,
        active_data: bpy_struct | None,
        active_property: str,
    ) -> None: ...
```
