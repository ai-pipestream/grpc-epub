# grpc-epub design

## 1. Goals

- Feature parity with Docling `InputFormat.EPUB`.
- Diskless unpack. Docling writes a temp dir for images; we keep
  entries in RAM under a hard uncompressed cap.
- Preserve spine order. Chapters become `GroupItem`s in that order
  after the HTML collector returns.
- Stream chapter HTML as soon as that zip entry is read, so a 50 MB
  EPUB does not wait for the last image.

## 2. Non-goals (v1)

- EPUB 3 media overlays / SMIL narration (that is ASR's job if we
  ever ingest the audio).
- Writing EPUB.
- Full CSS paged media.
- Nested ZIP / ZIP-in-ZIP.

## 3. Wire API (sketch)

`ai.pipestream.epub.v1.EpubParseService`

```text
rpc ParseEpub(stream ParseEpubRequest) returns (stream ParseEpubEvent);
rpc GetServiceInfo(GetServiceInfoRequest) returns (ServiceInfo);
```

Options:

- `max_document_mib` — compressed upload
- `max_uncompressed_mib` — zip bomb cap
- `max_entries`
- `include_images` (default true)

Events:

1. `EpubInfo` — title, creators, language, identifier, spine length
2. `Chapter` — spine index, href, media-type, XHTML/HTML bytes
3. `Resource` — href, media-type, bytes (images, css skipped unless
   requested)
4. `ParseStatus`

gRParse concatenates: for each `Chapter`, call the HTML collector
(in-process mapper or `grpc-lol-html` Document projection) with a
base href so relative images resolve against `Resource`s already
seen. Forward references (image after chapter) are allowed: the
coordinator buffers resources until the chapter mapper asks, then
drops them.

## 4. Mapping to Document

| EPUB | Document |
|---|---|
| OPF title/creators | origin metadata |
| spine item | `GroupItem` (chapter) whose children are the HTML collector's items |
| img in XHTML | `PictureItem` with `ImageRef` from the matching resource |
| NCX / nav | optional `GroupItem` of links; not required for v1 parity |

`CollectorSource.collector = "epub"` on the groups; HTML-derived
items keep `html` as well so merge stays additive.

## 5. Zip policy

- Refuse encrypted entries.
- Refuse compression methods other than store/deflate.
- Running uncompressed byte counter; exceed cap →
  `RESOURCE_EXHAUSTED` without finishing the extract.
- Path traversal (`../`, absolute paths) → `INVALID_ARGUMENT`.

## 6. Tests

- Minimal EPUB (one chapter, one PNG) built in the test, no binary
  committed. Assert title, one chapter href, image bytes round-trip.
- Zip bomb (small compressed, huge uncompressed) hits the cap.
- Missing OPF / empty spine → `INVALID_ARGUMENT`.
- Chapter HTML with a relative image resolves after the `Resource`
  event, not before (ordering test).
