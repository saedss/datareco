# Structure templates

A template tells DataReco how to read a structure on disk: which bytes are which
field, what type each field is, and what its values mean. The disk editor's
Structure panel and `datareco decode` use them.

DataReco ships templates for MBR/EBR, GPT, FAT12/16/32, exFAT, NTFS and ext2/3/4
([crates/templates/src/builtin](../crates/templates/src/builtin)). You can add your
own: put `.toml` files in your templates folder (the Structure panel's **Open
folder** button shows where; on Windows it's
`%APPDATA%\io.github.saedss.datareco\templates`) and click **Reload**. A template
with the same id as a built-in one replaces it. On the command line, pass
`--templates-dir FOLDER`.

Decoding never changes the disk, and a template can't crash DataReco: bytes that
are missing or don't make sense show up as fields marked with a problem.

## A small example

```toml
[[template]]
id = "my_header"               # letters, digits and _
name = "My file header"
group = "Mine"                 # groups templates in lists
description = "Shown as a tooltip."
size = 16                      # bytes, if the structure has a fixed size
signature = [{ offset = 0, ascii = "MYHD" }]  # suggest it when these bytes match

[[template.field]]
name = "Magic"
offset = 0
type = "ascii"
length = 4

[[template.field]]
name = "Version"
offset = 4
type = "u16"
values = { "1" = "first version", "2" = "second version" }

[[template.field]]
name = "Flags"
offset = 6
type = "u16"
format = "hex"
flags = { "0x1" = "compressed", "0x2" = "encrypted" }

[[template.field]]
name = "Name length"
offset = 8
type = "u8"

[[template.field]]
name = "Name"
offset = 9
type = "ascii"
length = "name_length"         # an expression using an earlier field
```

## Templates

| Key | Meaning |
|---|---|
| `id` | Unique name, used to refer to the template |
| `name` | What people see |
| `group` | Heading it's listed under |
| `description` | Longer explanation |
| `size` | Fixed size in bytes. Needed for templates used in arrays |
| `extent` | Bytes to read when decoding it on its own (default: `size`, else 4096) |
| `signature` | List of `{ offset, hex = "55 AA" }` or `{ offset, ascii = "NTFS" }`. All must match for the template to be suggested |
| `summary` | One-line description of a decoded structure, e.g. `"{name}.{extension}"`. `{field_id}` is replaced by that field's value (or its meaning, if it has one) |
| `list` | `false` hides helper templates from lists |

## Fields

Every field has a `name`, an `offset` from the start of the structure, and a
`type`. Its `id` (for expressions and summaries) is the name in lower case with
`_` for spaces and punctuation, e.g. "Bytes per sector" → `bytes_per_sector`; set
`id` to choose another. `description` becomes a tooltip; `hidden = true` keeps a
field out of the display but still usable in expressions.

| Type | Bytes | Shown as |
|---|---|---|
| `u8` `u16` `u24` `u32` `u48` `u64` | 1–8 | unsigned integer, little-endian |
| `i8` `i16` `i32` `i64` | 1–8 | signed integer |
| `u16be` `u32be` … | | big-endian versions |
| `bytes` | `length` | hex bytes |
| `ascii` | `length` | text up to the first NUL |
| `utf16` | `length` (in bytes) | UTF-16LE text up to the first NUL |
| `guid` | 16 | Microsoft GUID (`EBD0A0A2-B9E5-…`) |
| `uuid` | 16 | UUID as stored by Linux file systems |
| `chs` | 3 | MBR cylinder/head/sector address |
| `fat_date`, `fat_time` | 2 | FAT date or time |
| `fat_datetime` | 4 | exFAT timestamp (time in the low half) |
| `filetime` | 8 | Windows FILETIME (100 ns since 1601), UTC |
| `unix_time` | 4 | seconds since 1970, UTC |
| `struct` | | another template (see below) |
| `chain` | | a list of variable-length items (see below) |

Integer fields can have:

- `format = "hex"` to show `0x1F` instead of `31`;
- `values = { "0x07" = "NTFS" }` to name particular values (keys are decimal or `0x` hex);
- `flags = { "0x1" = "read-only", "0x10" = "directory" }` to name the bits that are set.

`guid` fields can have `values` too, keyed by the GUID text.

### Structures and arrays

```toml
[[template.field]]
name = "Partitions"
offset = 446
type = "struct"
template = "mbr_partition"
count = 4            # or an expression, or "fill" for as many as fit
```

Without `count` it's one nested structure. `inline = true` shows its fields in
place of the structure (useful with `select`).

To choose the template by a value, use `select` instead of `template`:

```toml
[[template.field]]
name = "Entry"
offset = 0
type = "struct"
inline = true
select = { value = "attributes & 0x3F == 0x0F", cases = { "1" = "fat_lfn_entry" }, default = "fat_short_entry" }
```

If no case matches and there's no `default`, the field is left out.

### Chains

For lists of variable-length items, like NTFS attributes:

```toml
[[template.field]]
name = "Attributes"
offset = "first_attribute"       # where the first item starts
type = "chain"
template = "ntfs_attribute"
item_length = "length"           # each item's length, from its own fields
until = "type == 0xFFFFFFFF"     # stop at an item where this is true
max = 64                         # never more items than this (default 64)
```

## Expressions

`offset`, `length`, `count`, `select.value`, `item_length` and `until` can be
expressions over earlier fields of the same structure, e.g. `name_length * 2`.

- Numbers: decimal, `0x` hex, `0b` binary.
- Operators, from lowest to highest precedence: `== != < <= > >=`, `|`, `^`, `&`,
  `<< >>`, `+ -`, `* / %`, unary `-`, and parentheses.
- Comparisons give 1 or 0. Unlike C, `a & b == c` means `(a & b) == c`.
