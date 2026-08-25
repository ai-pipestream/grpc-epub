# grpc-epub design

## 1. Goals

- Diskless unpack: entries stay in RAM under a hard uncompressed cap.
- Preserve spine order: chapters become `GroupItem`s in that order after the
  HTML collector returns.
- Stream chapter XHTML as soon as that zip entry is read, so a 50 MB EPUB does
  not wait for the last image. Live UI is the contract: paint spine order as it
  unzips. A unary Document convenience RPC is not the path UIs use.

## 2. Non-goals (v1)

- Transcribing narration audio (that is ASR's job if we ever ingest the audio).
  The *alignment* is a different question: an EPUB 3 media overlay ships the
  text-to-audio mapping already authored in the book, so reading it needs no
  model and is done here, under `ParseOptions.parse_media_overlays`.
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

- `max_document_mib`: compressed upload
- `max_uncompressed_mib`: zip bomb cap
- `max_entries`
- `include_images` (default true)

Events:

1. `EpubInfo`: title, creators, language, identifier, spine length
2. `Chapter`: spine index, href, media-type, XHTML/HTML bytes
3. `Resource`: href, media-type, bytes (images, css skipped unless requested)
4. `ParseStatus`

gRParse concatenates: for each `Chapter`, call the HTML collector (in-process
mapper or the `grpc-lol-html` Document projection) with a base href so relative
images resolve against `Resource`s already seen. Forward references (image
after chapter) are allowed: the coordinator buffers resources until the chapter
mapper asks, then drops them.

## 4. Mapping to Document

| EPUB | Document |
|---|---|
| OPF title/creators | origin metadata |
| spine item | `GroupItem` (chapter) whose children are the HTML collector's items |
| img in XHTML | `PictureItem` with `ImageRef` from the matching resource |
| NCX / nav | optional `GroupItem` of links; not required for v1 |

`CollectorSource.collector = "epub"` on the groups; HTML-derived items keep
`html` as well so merge stays additive.

### 4.1 Implemented in this repo (`src/document_fold.rs`)

The mapping above is implemented here, behind `ParseOptions.emit_document`,
rather than only in gRParse. The fold is a single pass over the service's own
response events, the same messages that go on the wire, and the server emits
the result as one `document` event immediately before `status`.
`ai.pipestream.document.v1` is vendored byte-identical from gRParse into
`proto/ai/pipestream/document/v1/document.proto`; it is never edited here.

What is mapped:

| Event | Document |
|---|---|
| `info` | `Document.name` = title; `origin.mimetype` = `application/epub+zip`; `origin.binary_hash` = FNV-1a over the archive; `Document.source_meta` = `DocumentMeta{title, authors, created, modified, language, keywords, extra}`; the body group's `BaseMeta.language` and `.keywords`; and every OPF field under `epub.*` keys in the body group's `meta.custom_fields` (lists as `ListValue`, identifiers as `{value, scheme}` objects) |
| `navigation` | `Document.outline`, one `OutlineEntry{title, level, target}` per nav point, depth-first in reading order; `target` is a `FineRef` at the chapter group the entry names, unset when the href resolves to no spine item |
| `chapter` | one `GROUP_LABEL_CHAPTER` group under `#/body`, `name` = resolved href, meta `epub.idref` / `epub.media_type` / `epub.linear` / `epub.spine_index` / `epub.properties` / `epub.media_overlay_href` |
| `media_overlay` | `Document.media.duration_ms` = where the last cue ends, and the cues on the narrated chapter's group as `epub.media_overlay_cues` |
| `resource`, image | one `PictureItem` under `#/body`, `label = DOC_ITEM_LABEL_PICTURE`, `ImageRef{mimetype, uri = "epub:<href>"}`, meta `epub.href` / `epub.manifest_id` / `epub.cover` |
| `status` | nothing; it is a receipt of counts the items already imply |

**Typed slots now win, and the old keys stay for one release.** `dc:language`,
`dc:subject`, the title, the creators and the dates used to exist only as
`epub.*` custom fields, which the coordinator merges first-writer-wins: a
competing fragment silently kept its own. They now also fill
`Document.source_meta` and `BaseMeta.language` / `.keywords`, which the merge
and the query layer understand. The `epub.*` keys are still emitted so nothing
reading them breaks; treat them as deprecated for the facts that have a typed
home, and as the lossless tail for the ones that do not (`epub.rights`,
`epub.source`, `epub.type`, `epub.format`, `epub.coverage`, `epub.relation`,
`epub.creator_roles`).

The Document also carries the schema identifier declared by the vendored
schema, copied through verbatim:

```text
schema_name = "docling_document_v2"
```

What is deliberately not mapped, and why:

**Chapter contents.** The XHTML is never parsed here. The chapter groups are
emitted with no children on purpose: they are the sockets the HTML collector's
items merge into downstream, and an empty-children group is valid output (the
fold's integrity checker accepts it). Reimplementing HTML in the EPUB packager
is the thing this service exists not to do.

**Non-image resources.** Stylesheets, fonts, audio, video, nav documents, SMIL:
the schema has no item kind for them, labelling them as something else would be
a lie, and they are already on the typed stream in full for anyone who wants
them.

**Image bytes.** A Document is one gRPC message and clients commonly cap
receives at 4 MiB, so `ImageRef.uri` is a pointer, `epub:` plus the resolved
archive path, naming the `resource` event on this same stream that carries the
bytes. Not a data URI, not even for the cover. `ImageRef.size` is left unset
because nothing here decodes an image; a `Size` of 0x0 would be a claim rather
than a gap.

**Provenance.** No `prov` anywhere: an EPUB is reflowable and has no pages and
no bounding boxes. Source locators go in `meta.custom_fields` instead.

**`DocumentOrigin.filename`.** The server is handed bytes on a gRPC stream and
is never told what the file was called.

**Navigation beyond the `toc`.** The EPUB 3 navigation document and the EPUB 2
NCX are now parsed into `Document.outline`; the documents themselves are still
emitted as ordinary `resource` events carrying their bytes, unchanged. Only the
`toc` nav is read. `page-list` (printed page numbers, which `Document.pages`
and `ProvenanceItem.page_no` are shaped for) and `landmarks` are the same shape
and the same parser away, and are deferred only because neither has a wired-up
Document slot yet; reading them now would produce facts with nowhere to go.

**Cue-level media provenance.** SMIL overlays are parsed under
`ParseOptions.parse_media_overlays` and the cues go out in full on the typed
`media_overlay` event. On the Document plane the cues cannot yet land where
they belong: `SourceType.track` (`TrackSource{start_time, end_time,
identifier}`) hangs on text items, a cue addresses a fragment *inside* a
chapter, and this fold emits chapter groups with no children. Until the HTML
collector contributes those items downstream, the Document carries the
narration length in `MediaMeta.duration_ms` (a typed slot, a real fact) and the
cues as the `epub.media_overlay_cues` tail on the chapter group. Which chapters
have narration at all is a manifest fact and is reported either way, on
`Chapter.media_overlay_href` and `epub.media_overlay_href`, whether or not the
SMIL was parsed.

**Chapter to picture attribution.** Which chapter references an image is a fact
about the XHTML, so pictures hang off the body rather than off a chapter group.
The coordinator learns it from the HTML collector's own picture items.

Two conventions worth restating:

- `GroupItem` has no `source` field in this schema, so a chapter group cannot
  carry a `CollectorSource` the way a picture does; the same attribution rides
  in the group's meta under `epub.collector` as `{collector, version}`. In this
  schema `source` exists only on `DocItem`, `NodeItem` forbids extra fields,
  and namespaced custom meta fields are the sanctioned extension point, so this
  is the intended shape, not a workaround. Keeping one group per spine item
  also preserves chapter identity, which a fold into a single concatenated HTML
  string would lose. Pictures carry the real thing: `collector = "epub"`,
  `version` = the running build, no `model` (one engine) and no `confidence`
  (a declarative mapping has none to report).
- The `epub.*` book metadata is on the body group, and root meta is
  first-writer-wins in the coordinator's additive merge: if another collector's
  fragment lands first, those keys can be dropped. Per-item meta does not have
  that problem.

Ordering is arrival order. Chapters and resources interleave by archive
position and the fold appends as they come, never buffering or reordering; the
cover is still recognised because `info` names it and `info` is always first.

## 5. Zip policy

- Refuse encrypted entries.
- Refuse compression methods other than store/deflate.
- Running uncompressed byte counter; exceed cap and the call fails with
  `RESOURCE_EXHAUSTED` without finishing the extract.
- Path traversal (`../`, absolute paths) fails with `INVALID_ARGUMENT`.

## 6. Tests

- Minimal EPUB (one chapter, one PNG) built in the test, no binary committed.
  Assert title, one chapter href, image bytes round-trip.
- Zip bomb (small compressed, huge uncompressed) hits the cap.
- Missing OPF / empty spine fails with `INVALID_ARGUMENT`.
- Chapter HTML with a relative image resolves after the `Resource` event, not
  before (ordering test).

## 7. What the implementation did differently

Recorded here rather than by editing the sketch above, so the reasoning
survives review.

**`ParseEpubEvent` is `ParseEpubResponse`.** buf's `STANDARD` lint set requires
an RPC's response type to be named `<Rpc>Response`, and the fleet rule is that
lint runs clean without comment ignores. The oneof inside is unchanged.
`GetServiceInfo` returns `GetServiceInfoResponse` for the same reason, wrapping
the fields section 3 called `ServiceInfo`.

**Two more options than section 3 lists.** `max_compression_ratio`, because a
total cap alone lets an attacker sit just under it and still buy a
thousandfold amplification, and `include_all_resources`, so fonts and media are
reachable without a boolean per kind.

**`include_images` and `include_stylesheets` are `optional bool`.** Section 3
wants images on by default, and proto3 gives a bare bool no way to tell "the
caller said false" from "the caller said nothing".

**Non-spine markup is always emitted.** A nav document or an NCX is small and
is what a reader needs to build navigation, so it is not gated behind the image
or stylesheet options. Fonts, audio, video and everything else are off unless
`include_all_resources` is set.

**Broken spine references fail before the first event.** Section 5 does not say
when. The central directory lists every file up front, so a dangling `idref` or
a missing chapter is diagnosed before the stream opens rather than after three
chapters have been delivered.

**Resource ordering is by archive position.** Section 3 allows a resource to
arrive after the chapter referencing it. The implementation emits each resource
at the point its archive entry is reached during the spine walk, which is
deterministic per file and is what `architecture.md` means by "when their
entries are hit". The `tests/parse_epub.rs` ordering test pins it by packing
the same book two ways.

**The DRM split.** `META-INF/encryption.xml`, or an entry with the encryption
bit set, is `UNIMPLEMENTED`, matching the ownership table in `architecture.md`.
A ZIP that never claimed to be an EPUB is also `UNIMPLEMENTED`; a file that
says `application/epub+zip` and then has no container is `INVALID_ARGUMENT`,
because that is a broken book rather than an unsupported format.
