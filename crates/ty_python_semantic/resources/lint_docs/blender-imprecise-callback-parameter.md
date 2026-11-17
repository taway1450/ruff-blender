## What it does

Checks the parameters of a Blender callback override (`UIList.draw_item`, `Operator.description`,
...) for an annotation narrower than the parent's parameter type: a subtype of it, without `None`,
such as the add-on's own `PropertyGroup` for `draw_item`'s `item` or `Armature` for its `data`.

The override is accepted, and its body sees the annotated type. An annotation that is no subtype of
the parent's type is still ty's `incompatible-method-override`.

The rule belongs to the category `blender-imprecise-annotations`.

## Why is this bad?

A narrower parameter type breaks the override for callers that go through the parent's type. Only
Blender calls a callback, and it passes what the add-on gave it (`template_list(data, ...)`), so the
annotation is right as long as the add-on passes that type: the typechecker cannot check that.

## Examples

```py
from bpy.types import Armature, UIList


class MY_UL_layers(UIList):
    def draw_item(self, context, layout, data: Armature, item, icon, active_data, active_property):  # error: [blender-imprecise-callback-parameter]
        layout.label(text=data.name)
```

Leave the parameter unannotated (it has the parent's type) and check its type where it matters.

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
class bpy_struct:
    name: str

class Context(bpy_struct): ...
class Armature(bpy_struct): ...

class UILayout(bpy_struct):
    def label(self, *, text: str = "") -> None: ...

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
