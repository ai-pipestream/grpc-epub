# grpc-epub architecture

**Status:** spec (no implementation yet)
**Updated:** 2026-08-13

## Where this sits

An EPUB is a ZIP of XHTML plus an OPF spine. Docling's backend unpacks
to a temp dir and delegates each chapter to `HTMLDocumentBackend`.
This service does the same split **without a temp dir**: zip in
memory, spine in order, HTML fragments to the HTML collector.

```text
.epub bytes
        │
        ▼
   grpc-epub           OPF metadata + spine + unzipped XHTML/images
        │
        ├─ metadata / chapter list  ──►  Document shell
        └─ chapter XHTML            ──►  HTML collector (gRParse)
        └─ images                   ──►  PictureItem / CV if needed
```

This process is deliberately thin. It owns packaging, not HTML
semantics.

## Live results (vs Docling)

Docling unpacks the EPUB, runs every chapter through the HTML backend,
then returns one document. We emit `EpubInfo` (title, spine length)
immediately, then **each chapter's XHTML as that zip entry is read**,
so a UI can show chapter 1 while chapter 12 is still in the archive.
Images stream as `Resource` events when their entries are hit, not at
the end. `ParseStatus` is a trailer.

## What this process owns

- ZIP bomb limits (entry count, uncompressed cap, nested zip
  refused) — Docling's `EpubBackendOptions` equivalent, enforced
  in memory.
- `META-INF/container.xml` → OPF → metadata (title, creators,
  language, identifiers) and the **spine order**.
- Streaming each spine item's bytes as a chapter event.
- Resolving internal image paths to bytes for `ImageRef`s the HTML
  mapper can attach.

## What this process does not own

| Concern | Owner |
|---|---|
| HTML reading order, headings, tables, links | HTML collector |
| CSS layout / pagination | out of scope (EPUBs are reflow) |
| DRM / Adobe ADE | `UNIMPLEMENTED` |
| Export to a new EPUB | protomolt, maybe never |

## Language

**Rust**. `zip` crate over `Cursor<&[u8]>`, XML via `quick-xml` for
container/OPF only. No filesystem. Matches `grpc-calamine` /
`grpc-lol-html` operationally: diskless, streaming, read-only
container.
