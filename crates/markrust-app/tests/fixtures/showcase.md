---
title: MarkRust Editor Showcase
author: Test Suite
date: 2026-08-28
tags: [markdown, editor, showcase]
---

# MarkRust Editor Showcase

This document exercises the full range of Markdown rendering features in MarkRust. Headings should step down in size, lists should indent, quotes should show a bar, and fenced code should sit on a distinct background.

> Native Markdown on disk. WYSIWYG in the editor.

## Text Formatting

Regular body text with **bold emphasis**, *italic styling*, and ~~strikethrough~~ for deleted content. You can also combine **_bold italic_** formatting in a single span.

### Links and Images

Visit the [MarkRust homepage](https://markrust.example.com) or read the [documentation](./docs/README.md).

![Placeholder landscape](assets/icon/icon.png)

## Lists

### Bullet List

- First item with **bold** text
- Second item with a [link](https://example.com)
- Third item with nested content:
  - Nested alpha
  - Nested beta

### Task List

- [x] Create test markdown file
- [x] Build MarkRust locally
- [ ] Capture screenshot
- [ ] Write visual assessment

## Blockquote

> "The best way to predict the future is to invent it."
>
> — Alan Kay

Blockquotes can span multiple paragraphs and include **inline formatting**.

## Code Blocks

### Rust

```rust
fn main() {
    let greeting = "Hello, MarkRust!";
    println!("{greeting}");
}
```

### JSON

```json
{
  "name": "MarkRust",
  "version": "0.1.0",
  "features": ["syntax-highlighting", "live-preview", "workspace"]
}
```

## Table

| Feature        | Status   | Notes                    |
|----------------|----------|--------------------------|
| Headings       | ✅       | H1 through H6            |
| GFM Tables     | ✅       | Aligned columns          |
| Task Lists     | ✅       | Checkbox rendering       |
| Code Highlight | ✅       | Rust, JSON, and more     |
| Frontmatter    | ✅       | YAML metadata block      |

## Closing Paragraph

MarkRust aims to be a fast, native Markdown workspace for macOS. This sample file covers the most common authoring patterns — headings, inline styles, lists, quotes, fenced code, and tables — so you can quickly judge readability, spacing, and overall polish in both the editor and preview panes.
