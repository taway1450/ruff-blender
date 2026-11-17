## What it does

Checks the result of an operator's `execute`, `invoke` and `modal` (and of any other Blender
callback whose result is a set of names) for a type less precise than the set of names Blender
accepts:

- a return annotation that is the parent's result with the names widened to `str`: `-> set[str]` or
  `-> typing.Set[str]` where the stubs say `set[Literal["RUNNING_MODAL", "CANCELLED", ...]]`. This
  also covers a mixin's method that a Blender class gets the callback from;
- a returned `set[str]` that is no set display, such as a set held in a variable
  (`ret = {"CANCELLED"}` ... `return ret`).

The code is accepted, and typed as the precise form: a method annotated `-> set[str]` has the
parent's result type for its callers, and its `return` statements are checked against it, so a
name that is not a result (`return {"FINSHED"}`) is still an `invalid-return-type` error.

The rule belongs to the category `blender-imprecise-annotations`.

## Why is this bad?

The names are `Literal` types only in the stubs; Blender checks each name itself when the method
returns and raises for one that is not a result. A `set[str]` hides a misspelt name from the
typechecker until Blender runs the operator.

## Examples

```py
from typing import Literal

from bpy.types import Operator


class MY_OT_run(Operator):
    bl_idname = "my.run"
    bl_label = "Run"

    def execute(self, context) -> set[str]:  # error: [blender-imprecise-operator-return]
        return {"FINSHED"}  # error: [invalid-return-type]

    def invoke(self, context, event):
        result = {"CANCELLED"}
        return result  # error: [blender-imprecise-operator-return]
```

Annotate the method with the precise result type, or leave the annotation out: an unannotated
override has its parent's result type.

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
class Context(bpy_struct): ...
class Event(bpy_struct): ...

class Operator(bpy_struct):
    bl_idname: str
    bl_label: str
    def execute(self, context: Context) -> set[Literal["FINISHED", "CANCELLED"]]: ...
    def invoke(self, context: Context, event: Event) -> set[Literal["FINISHED", "CANCELLED"]]: ...
```
