## What it does

Checks the metadata Blender reads before it loads an add-on: the `bl_info` dict of a legacy add-on, and the
`blender_manifest.toml` of an extension.

- `bl_info` must be a dict display with a `"name"` entry, and its `"blender"` entry must be a tuple of three
  ints.
- `blender_manifest.toml` must be valid TOML with the keys `schema_version`, `id`, `version`, `name`,
  `tagline`, `maintainer`, `type`, `blender_version_min` and `license`, and `type` must be `"add-on"` or
  `"theme"`.

## Why is this bad?

Blender refuses to list or install an add-on whose metadata is missing or malformed, so the mistake only shows
when someone tries to enable it.

Problems in `bl_info` are reported on the name `bl_info` or on the entry. Problems in
`blender_manifest.toml` are reported at the start of the add-on's `__init__.py`, because a TOML file
cannot carry a `# ty: ignore` comment.

## Examples

`my_addon/__init__.py`:

```py
bl_info = {  # error: [blender-invalid-addon-metadata] "has no"
    "version": (1, 0, 0),
    "blender": (4, 2),  # error: [blender-invalid-addon-metadata] "three ints"
}

def register(): ...
def unregister(): ...
```
