# Desktop diagnostics

MarkRust writes a small local diagnostic journal to
`~/Library/Logs/MarkRust/diagnostics.jsonl` on macOS. The two rotated files,
`diagnostics.jsonl.1` and `.2`, retain older entries. Each file is limited to
1 MiB, for a 3 MiB maximum. The directory is private (`0700`) and the files
are readable only by the account owner (`0600`).

The journal records app startup and clean shutdown, window creation,
document-open and HTML-export outcomes, and Rust panics. Entries contain time, process and
session identifiers, app version,
fixed event/error categories, and a panic source filename and line. They do
**not** contain document text, document paths, URLs, typed keys, arbitrary
error messages, or full backtraces. Logging is best-effort: an unavailable
directory does not prevent MarkRust from opening.

After a crash, use Finder's **Go → Go to Folder…** and enter
`~/Library/Logs/MarkRust`. Check the end of `diagnostics.jsonl` and, if
needed, the rotated files. The older `panics.log` is preserved and its file
permissions are tightened on the next launch; it may contain sensitive
payloads from previous versions, so review it before sharing.

Rust's panic hook cannot catch every failure. For a native or OS-level crash,
also check `~/Library/Logs/DiagnosticReports` for a MarkRust/markrust report.
Those system reports may contain private paths and should likewise be reviewed
before sharing.

## Related

- [Engineering documentation](README.md) — reference index
- [WYSIWYG engineering notes](roadmap.md) — development and debugging context
