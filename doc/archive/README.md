# Historical documentation

These records preserve earlier investigations. Their results apply to the
versions and workloads stated in each document. They are not current operating
instructions or evidence that later audit findings have been fixed.

| Record | Historical scope | Current reference |
| --- | --- | --- |
| [Code audit, 2026-11-11](code-audit.md) | Earlier fixes, module extraction and validation | [Audit summary](../audit_results.md) |
| [Message index investigation](message-text-index.md) | Schema 30–34 changes and synthetic connect measurements | [Current message storage](../message-text-index.md) |
| [Memory layout and reader review](performance-layout.md) | Rust 1.99.1 layouts, reader timing and proposed optimization analysis | [Performance measurement](../performance-layout.md) |

Do not apply the archived storage guide's manual index creation or rollback
commands to the current schema. soju-rs manages required indexes through its
migrations.

[Documentation index](../README.md)
