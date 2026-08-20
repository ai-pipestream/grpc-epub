# Sample EPUBs

The small books here are **generated, not committed** — this repository keeps
no binaries. Regenerate them with:

```bash
cd ../node-client
npm run samples
```

| File | What it is |
|---|---|
| `two-chapters.epub` | the default two-chapter fixture from `tests/common/mod.rs`, cover image stored after the chapters |
| `cover-first.epub` | the same book with the cover stored before the chapters, so its `resource` event arrives first |
| `long-book.epub` | twelve padded chapters, enough to watch the spine stream |

`large/` is gitignored and meant for real books (a Project Gutenberg download,
say) that do not belong in the repository. Anything `.epub` dropped there
appears in the viewer's dropdown.
