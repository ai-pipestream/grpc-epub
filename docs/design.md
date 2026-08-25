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
| `info` | `Document.name` = title; `origin.mimetype` = `application/epub+zip`; `origin.binary_hash` = FNV-1a over the archive; `Document.source_meta` = `DocumentMeta{title, authors, created + created_raw, modified + modified_raw, language, keywords, extra}`; the body group's `BaseMeta.language` and `.keywords`; and the OPF fields that have no typed home under `epub.*` keys in the body group's `meta.custom_fields` (lists as `ListValue`, identifiers as `{value, scheme}` objects) |
| `navigation` | `Document.outline`, one `OutlineEntry{title, level, target}` per nav point, depth-first in reading order; `target` is a `FineRef` at the chapter group the entry names, unset when the href resolves to no spine item |
| `chapter` | one `GROUP_LABEL_CHAPTER` group under `#/body`, `name` = resolved href, meta `epub.idref` / `epub.media_type` / `epub.linear` / `epub.spine_index` / `epub.properties` / `epub.media_overlay_href` |
| `media_overlay` | `Document.media.duration_ms` = where the last cue ends, and the cues on the narrated chapter's group as `epub.media_overlay_cues` |
| `resource`, image | one `PictureItem` under `#/body`, `label = DOC_ITEM_LABEL_PICTURE`, `ImageRef{mimetype, uri = "epub:<href>"}`, meta `epub.href` / `epub.manifest_id` / `epub.cover` |
| `status` | nothing; it is a receipt of counts the items already imply |

**Typed slots win, and the old keys are gone.** `dc:language`,
`dc:subject`, the title, the creators and the dates once existed
only as `epub.*` custom fields, which the coordinator merges
first-writer-wins: a competing fragment silently kept its own. The previous
wave gave each of them a first-class slot and kept the old key beside it for
one release. **That release has passed: the duplicates are gone.** A fact the
schema types is now written to its typed field and to nothing else, because a
string beside a typed field is a second answer with no tiebreaker, and because
the canonical schema says so at `DocumentMeta.extra` itself ("Data whose shape
the fleet knows gets a typed field, never an entry here").

Removed from the body group's `meta.custom_fields`, each because the fact now
has a home the merge and the query layer understand:

| Gone | Now |
|---|---|
| `epub.language` | `BaseMeta.language` (`LanguageMetaField`) and `DocumentMeta.language` |
| `epub.subjects` | `BaseMeta.keywords` (`KeywordsMetaField`) and `DocumentMeta.keywords` |
| `epub.creators` | `DocumentMeta.authors` |
| `epub.date` | `DocumentMeta.created` + `.created_raw` |
| `epub.modified` | `DocumentMeta.modified` + `.modified_raw` |

**The dates are instants.** `DocumentMeta.created` and `.modified` are
`google.protobuf.Timestamp`s and the schema states the contract for the pair:
the typed field carries the parsed instant, the `_raw` twin keeps the source's
own spelling, and the twin is the only field set when the value does not parse.
`src/datetime.rs` is the reader, hand-written because the crate deliberately
carries no date library (`zip` is built without its `time` feature) and because
what is needed is one fixed grammar plus an integer calendar conversion.
`created` prefers `dcterms:created`, which is a claim about creation, and falls
back to `dc:date`, which is what an EPUB 2 book has instead and means "a date
associated with an event in the life cycle of the resource". The raw twin is
written whenever the book stated a date at all, not only on failure: reading
`1843-10-01` as an instant means taking W3CDTF's UTC default for an offset the
book never wrote, and the twin is what makes that reading reversible.

**`extra` keeps the open vocabulary, and it is the right type for it.**
`epub.publisher`, `epub.description`, `epub.rights`, `epub.source`,
`epub.type`, `epub.format`, `epub.coverage`, `epub.relation`, the
`epub.title.<title-type>` subtitles, the `epub.role.<name>` MARC relators and
`epub.file-as.<name>` sort names, `epub.unique_identifier`, `epub.version` and
`epub.opf_href` all stay. None of them has a typed field in the schema, and
none of them should: a MARC relator list runs to hundreds of codes, a producer
may invent any `<meta property="…">` it likes, and Dublin Core's
`rights`/`source`/`type`/`format`/`coverage`/`relation` are prose by design.
`epub.creator_roles` stays on the body group for the same reason.

One thing did change shape inside `extra`. Every `dc:identifier` used to be
squashed into a single space-joined `epub.identifiers` value; each now has its
own key, `epub.identifier.<n>` with `epub.identifier.<n>.scheme` beside it,
because an identifier is free to contain a space and a reader after the third
one should not have to know this fold's separator.

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
them. `Document.attachments` was weighed and left empty rather than filled with
these. A `SubDocumentRef` is a nested payload "addressable for fan-out
parsing", and the resources here that anything would ever fan out to parse are
exactly the two this service already parses itself, whose *results* are the
projection: the nav document is `Document.outline` and the overlays are
`Document.media`. A stylesheet, a font or an audio track is not a sub-document
and would not be parsed by anyone downstream, so listing them there would be a
plausible-looking slot filled with things that do not belong in it.

**`DocumentMeta.generator` and `.schema_location`.** Both left unset, both
deliberately. An OPF states no producing software: the closest thing is a
`bkp` (book producer) credit, which is as often a person or an imprint as it is
a program, so reading one as a generator would be an invention rather than a
reading. And a package document declares no grammar for itself; which one it
follows is fixed by the EPUB version, which is `epub.version`, not by an
`xsi:schemaLocation`.

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
