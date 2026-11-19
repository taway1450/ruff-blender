## What it does

Checks the reads of `context.object` and `context.active_object` on a `bpy.types.Context` after a
call of an operator that adds an object (`bpy.ops.object.add()` or any `bpy.ops.<category>.*_add()`,
such as `bpy.ops.mesh.primitive_cube_add()`) earlier in the same function, or earlier in a
module-level script.

The read is typed without `None`, and as the Type tag view of the object the operator adds where
the call shows its type and the stubs have the view: `_Object_MESH` after
`bpy.ops.mesh.primitive_cube_add()`, `_Object_LATTICE` after `bpy.ops.object.add(type='LATTICE')`,
so its `data` is a `Mesh` or a `Lattice`. A read that the code only tests for truth or `None` is
not reported.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

An operator that adds an object selects the new object and makes it the active one, so code reads
it back from the context right after the call. Whether the call succeeded is not visible: an
operator whose poll fails raises, or returns `{'CANCELLED'}` when it cannot add the object, and
then the active object is whatever it was before, possibly `None`.

## Examples

```py
import bpy


def add_cube(context: bpy.types.Context) -> str:
    bpy.ops.mesh.primitive_cube_add()
    return context.object.name  # error: [blender-assumed-added-object]
```

Check the result of the call, or the object, before using it.

The example uses these minimal Blender stubs:

```toml
[environment]
python = "/.venv"
```

`/.venv/<path-to-site-packages>/bpy/__init__.pyi`:

```pyi
from . import ops as ops
from . import types as types
```

`/.venv/<path-to-site-packages>/bpy/ops/__init__.pyi`:

```pyi
from . import mesh as mesh
```

`/.venv/<path-to-site-packages>/bpy/ops/mesh.pyi`:

```pyi
def primitive_cube_add() -> set[str]: ...
```

`/.venv/<path-to-site-packages>/bpy/types.pyi`:

```pyi
class bpy_struct: ...

class Object(bpy_struct):
    name: str

class Context(bpy_struct):
    object: Object | None
```
