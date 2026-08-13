# grpc-epub design

## 1. Goals

- Feature parity with Docling `InputFormat.EPUB`.
- Diskless unpack. Docling writes a temp dir for images; we keep
  entries in RAM under a hard uncompressed cap.
- Preserve spine order. Chapters become `GroupItem`s in that order
  after the HTML collector returns.
- Stream chapter HTML as soon as that zip entry is read, so a 50 MB
  EPUB does not wait for the last image. Live UI is the contract:
  Docling's EPUB backend is batch; we paint spine order as it unzips.
  A unary Document convenience RPC is not the path UIs use.

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

## 7. What the implementation did differently

Recorded here rather than by editing the sketch above, so the reasoning
survives review.

- **`ParseEpubEvent` is `ParseEpubResponse`.** buf's `STANDARD` lint set
  requires an RPC's response type to be named `<Rpc>Response`, and the
  fleet rule is that lint runs clean without comment ignores. The oneof
  inside is unchanged. `GetServiceInfo` returns
  `GetServiceInfoResponse` for the same reason, wrapping the fields
  §3 called `ServiceInfo`.
- **Two more options than §3 lists.** `max_compression_ratio`, because
  a total cap alone lets an attacker sit just under it and still buy a
  thousandfold amplification, and `include_all_resources`, so fonts and
  media are reachable without a boolean per kind.
- **`include_images` and `include_stylesheets` are `optional bool`.**
  §3 wants images on by default, and proto3 gives a bare bool no way to
  tell "the caller said false" from "the caller said nothing".
- **Non-spine markup is always emitted.** A nav document or an NCX is
  small and is what a reader needs to build navigation, so it is not
  gated behind the image or stylesheet options. Fonts, audio, video and
  everything else are off unless `include_all_resources` is set.
- **Broken spine references fail before the first event.** §5 does not
  say when. The central directory lists every file up front, so a
  dangling `idref` or a missing chapter is diagnosed before the stream
  opens rather than after three chapters have been delivered.
- **Resource ordering is by archive position.** §3 allows a resource to
  arrive after the chapter referencing it. The implementation emits
  each resource at the point its archive entry is reached during the
  spine walk, which is deterministic per file and is what
  `architecture.md` means by "when their entries are hit". The
  `tests/parse_epub.rs` ordering test pins it by packing the same book
  two ways.
- **The DRM split.** `META-INF/encryption.xml`, or an entry with the
  encryption bit set, is `UNIMPLEMENTED`, matching the ownership table
  in `architecture.md`. A ZIP that never claimed to be an EPUB is also
  `UNIMPLEMENTED`; a file that says `application/epub+zip` and then has
  no container is `INVALID_ARGUMENT`, because that is a broken book
  rather than an unsupported format.
