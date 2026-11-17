## What it does

Checks the reads of a member that a refined base of `bpy.types` (a class with a Type tag, such as
`Constraint`, `Modifier`, `Light`, `Node` or `NodeSocket`) lacks, but some of the subclasses its
tag names declare: `ob.constraints[0].target`, `bpy.data.lights["Key"].spot_size`,
`node.inputs["Color"].default_value`.

The read is typed as the union of those subclasses' declarations of the member. A member no
subclass declares is still an error, and so is a member read on `bpy_struct`, `ID` or a class
without a Type tag.

The rule belongs to the category `blender-assumptions`.

## Why is this bad?

Blender always hands out a struct of a refined base as the subclass its Type tag names, so the
member is there when the struct is one of the subclasses that declare it. A lookup by index or by
name does not say which one it is: the code takes for granted that the first constraint has a
target, or that the light called "Key" is a spot light.

## Examples

```py
import bpy


def first_target(ob: bpy.types.Object) -> bpy.types.Object | None:
    return ob.constraints[0].target  # error: [blender-assumed-subclass-member]


def copied_target(ob: bpy.types.Object) -> bpy.types.Object | None:
    con = ob.constraints[0]
    if con.type == 'COPY_LOCATION':
        return con.target  # ok: narrowed by the Type tag
    return None
```

Test the tag first, as `copied_target` does, so that the struct is narrowed to the subclass.

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

class Constraint(bpy_struct):
    type: Literal['COPY_LOCATION', 'LIMIT_SCALE']

class CopyLocationConstraint(Constraint):
    type: Literal['COPY_LOCATION']
    target: Object | None

class LimitScaleConstraint(Constraint):
    type: Literal['LIMIT_SCALE']

class ObjectConstraints(bpy_struct):
    def __getitem__(self, key: int | str) -> Constraint: ...

class Object(bpy_struct):
    constraints: ObjectConstraints
```
