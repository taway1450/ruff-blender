## What it does

Checks that a Blender add-on's root module (the package's `__init__.py`, or a single-file add-on) defines
`register` and `unregister`, and that both can be called with no arguments.

## Why is this bad?

Blender calls `register()` when the add-on is enabled and `unregister()` when it is disabled, with no
arguments. An add-on without one of them, or with one that requires a parameter, fails to enable or to
disable.

The functions are found the way Blender finds them: as module-level names of the root module, so an alias
(`register = _register`), an import from a submodule and `register, unregister =
bpy.utils.register_classes_factory(classes)` all count. Nothing is reported when the type of the name is unknown.

A missing function is reported at the start of the root module; a function with a wrong signature is reported
where the name is bound.

## Examples

`my_addon/__init__.py`:

```py
bl_info = {"name": "My add-on", "blender": (4, 2, 0)}  # error: [blender-invalid-register] "no `unregister`"

def register(context):  # error: [blender-invalid-register] "callable with no arguments"
    ...
```
