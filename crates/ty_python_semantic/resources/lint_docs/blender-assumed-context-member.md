## What it does

Checks the reads of `scene`, `view_layer` and `tool_settings` on a `bpy.types.Context` where
Blender does not guarantee them: outside a Blender callback (at module level, in a helper function,
a method of a class that is no Blender class, a nested function, an app handler or a timer), and
`scene` and `tool_settings` in a callback of an operator that a `FileHandler` of the same add-on
names as its `bl_import_operator`.

The read is typed without `None`. A read that the code only tests for truth or `None`
(`if bpy.context.scene:`) is not reported. Inside other Blender callbacks the members are always
set and the read is narrowed silently.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

Blender fills these members whenever it calls a callback. Elsewhere they hold the window's or the
loaded file's scene in ordinary use (a timer, a handler, a script, a test), but nothing promises
it: `context.temp_override(scene=None)` empties them, and the collection importer runs a file
handler's import operator with no scene. While an add-on is imported or registered, `bpy.context`
has no `scene` at all.

## Examples

```py
import bpy


def current_frame() -> int:
    return bpy.context.scene.frame_current  # error: [blender-assumed-context-member]
```

Take the context as a callback's parameter, or check the scene before using it.

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import types as types
from .types import Context

context: Context
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...

class Scene(bpy_struct):
    frame_current: int

class Context(bpy_struct):
    scene: Scene | None
```
