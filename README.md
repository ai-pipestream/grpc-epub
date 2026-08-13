# grpc-epub

A gRPC server that unpacks EPUB archives **in memory** and streams the spine
back chapter by chapter, as each entry comes off the ZIP.

It is a packager, not a document parser. It owns the ZIP, `META-INF/container.xml`,
the OPF package document and the spine; it hands chapter XHTML on as bytes for
the HTML collector to read. Nothing here parses, sanitizes, or rewrites HTML,
and nothing here touches disk.

```text
.epub bytes ──► grpc-epub ──► info      (title, creators, spine length)
                          ──► chapter   (spine order, one per itemref)
                          ──► resource  (images and markup, as their entries are hit)
                          ──► status    (counts and warnings; a trailer)
```

## Why it streams

Docling's EPUB backend unpacks to a temp directory, runs every chapter through
an HTML backend, and returns one document at the end. Here `info` goes out
before a single chapter has been inflated and each `chapter` goes out as its
entry is read, so a reader can paint chapter 1 while chapter 12 is still
compressed. `status` is a receipt, not the payload.

The upload itself *is* buffered, and that is the format's doing rather than a
choice: a ZIP's central directory sits at the end of the file, so nothing in an
archive can be located until its last byte has arrived. Streaming begins the
moment it is possible to begin.

`tests/streaming.rs` is the test that fails if someone turns this back into a
batch API. It watches the server's own counters: at the moment `info` reaches
the client, a streaming implementation has inflated almost nothing and a
batching one has inflated the whole book.

## Build and run

```sh
cargo build --release
cargo test                 # 77 tests, no network, no fixtures on disk
./target/release/grpc-epub # listens on 0.0.0.0:50051
```

Protobuf work goes through `buf`, never `protoc`:

```sh
buf lint                                       # STANDARD + COMMENTS, comment ignores disallowed
buf generate                                   # regenerate src/gen (protoc-gen-prost, protoc-gen-tonic)
buf build -o src/gen/file_descriptor_set.binpb # refresh the reflection descriptor
```

Container:

```sh
docker build -t grpc-epub .        # the test suite runs inside the build and gates it
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
  -p 50051:50051 grpc-epub
```

The runtime image is `distroless/cc` running as uid 65532, with no shell and no
package manager. `--read-only` is not a precaution here but a statement of
fact: the hot path never writes anything.

## Wire API

Package `ai.pipestream.epub.v1`, defined in
[`proto/ai/pipestream/epub/v1/`](proto/ai/pipestream/epub/v1). Server
reflection is registered, so `grpcurl -plaintext localhost:50051 list` works
against a live server without the `.proto` files.

```protobuf
service EpubParseService {
  rpc ParseEpub(stream ParseEpubRequest) returns (stream ParseEpubResponse);
  rpc GetServiceInfo(GetServiceInfoRequest) returns (GetServiceInfoResponse);
}
```

**Request.** The first frame must carry `options`; every frame after it carries
a `chunk` of archive bytes, in order. Half-close to signal the end of the
upload.

`ParseOptions` — every limit is clamped to the server's own ceiling and can
only ever be lowered from the wire. Zero means "use the server default".

| Field | Meaning |
|---|---|
| `max_document_mib` | compressed upload cap |
| `max_uncompressed_mib` | total inflated bytes for the call — the zip-bomb ceiling |
| `max_entries` | archive entry count |
| `max_compression_ratio` | per-entry inflated-to-stored ratio |
| `include_images` | emit image resources (absent = true) |
| `include_stylesheets` | emit CSS (absent = false) |
| `include_all_resources` | emit fonts, audio, video and everything else too (absent = false) |

**Response.** A successful stream is exactly one `info`, then `chapter` events
in spine order interleaved with `resource` events in archive order, then one
`status`.

| Event | Carries |
|---|---|
| `info` | title, creators, contributors, language, identifiers, publisher, date, subjects, spine length, OPF path, EPUB version, cover href |
| `chapter` | spine index, idref, resolved href, media type, **XHTML bytes verbatim**, `linear`, EPUB 3 properties |
| `resource` | resolved href, media type, kind, bytes, manifest id, properties |
| `status` | chapters and resources emitted, resources skipped, inflated bytes, entries read, warnings |

Non-spine markup (a nav document, an NCX) is always emitted; images are emitted
by default; stylesheets, fonts and media are opt-in. A resource is emitted
**when its archive entry is reached during the spine walk**, so a chapter may
reference a resource that has not arrived yet — buffer by href and resolve at
the end of the stream.

**Errors.** A failed stream ends with a gRPC status and no `status` event.
Events already delivered stay valid.

| Code | When |
|---|---|
| `RESOURCE_EXHAUSTED` | upload, entry count, total inflated size or an entry's compression ratio over its cap |
| `INVALID_ARGUMENT` | not a ZIP, truncated, path traversal in an entry name or href, or an EPUB whose `container.xml`, OPF or spine is missing or unusable |
| `UNIMPLEMENTED` | a ZIP that is not an EPUB, or one this build cannot open: DRM, entry encryption, or a compression method outside store and deflate |
| `INTERNAL` | a bug here; the parser panicked |

`grpc.health.v1.Health` is registered and reports
`ai.pipestream.epub.v1.EpubParseService` as `SERVING`.

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `GRPC_EPUB_ADDR` | `0.0.0.0:50051` | listen address |
| `GRPC_EPUB_WORKERS` | CPU count | tokio worker threads |
| `GRPC_EPUB_WINDOW_BYTES` | `4194304` | HTTP/2 initial stream and connection window |
| `GRPC_EPUB_METRICS_INTERVAL_SECS` | `60` | seconds between metrics lines on stdout; `0` disables |
| `GRPC_EPUB_MAX_DOCUMENT_MIB` | `256` | compressed upload ceiling |
| `GRPC_EPUB_MAX_UNCOMPRESSED_MIB` | `512` | total inflated ceiling per call |
| `GRPC_EPUB_MAX_ENTRIES` | `10000` | archive entry ceiling |
| `GRPC_EPUB_MAX_COMPRESSION_RATIO` | `200` | per-entry inflated-to-stored ceiling |
| `GRPC_EPUB_COMPRESSION_RATIO_FLOOR_BYTES` | `1048576` | size an entry must exceed before the ratio rule applies |
| `GRPC_EPUB_MAX_CHUNK_BYTES` | `16777216` | largest single inbound `chunk` frame |
| `GRPC_EPUB_MAX_CONCURRENT_PARSES` | `8` | calls that may inflate at once; further calls wait |

Every limit is also readable at runtime through `GetServiceInfo`.

Metrics are one line on stdout per interval:

```text
grpc-epub metrics parses_started=3 parses_succeeded=3 parses_failed=0 \
  chapters_emitted=214 resources_emitted=88 bytes_uploaded=4194304 bytes_inflated=9437184
```

## Hostile input

An EPUB is a ZIP full of XML supplied by whoever made the file, so the
interesting cases are not malformed books but well-formed hostile ones. Each
control has a test in `tests/security.rs` built by the test itself, so the
attack is legible in the source.

- **Decompression bombs.** Three rules, because each covers a hole the others
  leave: an entry-count check from the central directory, a running total of
  inflated bytes, and a per-entry inflated-to-stored ratio above a size floor.
  The last two are enforced against what actually comes out of the
  decompressor, not just against the sizes the archive declares, so a lying
  header is caught too. Over any of them is `RESOURCE_EXHAUSTED`, raised
  partway through the extract rather than after it.
- **Path traversal.** Entry names and OPF hrefs are percent-decoded, then
  normalized, then refused if they escape the archive root, are absolute, or
  contain a NUL or a backslash. Nothing here writes to disk, but the paths go
  out on the wire and a client that does write files would otherwise inherit
  the traversal.
- **XXE.** quick-xml has no DTD processor, so it cannot fetch an external
  entity. On top of that, a `<!DOCTYPE>` declaring an `<!ENTITY>` is refused
  outright, and any other general reference is copied through verbatim —
  `&xxe;` reaches the client as four literal characters. Both halves are
  asserted.
- **Encryption and DRM.** `META-INF/encryption.xml`, or any entry with the
  encryption bit set, is `UNIMPLEMENTED`.
- **Compression methods.** The `zip` crate is built without the features that
  decode anything but store and deflate, so the refusal is a build flag rather
  than a check that can be forgotten.
- **Nested archives.** Reported and never opened. Recursing is how a bomb hides
  from a single-level cap.
- **Remote resources.** Recorded as a warning and never fetched. There is no
  network on the parse path.

## Layout

| Path | What lives there |
|---|---|
| `proto/ai/pipestream/epub/v1/` | the wire contract; `buf lint` is the gate |
| `src/gen/` | `buf generate` output plus the reflection descriptor — never hand-edited |
| `src/archive.rs` | ZIP opening, the entry scan, and the zip-bomb budget |
| `src/opf.rs` | `container.xml` and OPF parsing, and the entity policy |
| `src/href.rs` | path normalization and the traversal policy |
| `src/extract.rs` | the parse driver and the emission order |
| `src/service.rs` | tonic wiring, upload handling, concurrency bound, panic supervisor |
| `src/limits.rs` | the ceilings and how a request is clamped to them |
| `tests/` | fixtures the tests author in memory; nothing binary is committed |

## Not in v1

EPUB 3 media overlays and SMIL narration, writing EPUB, CSS paged media,
ZIP-in-ZIP, and DRM. `docs/design.md` has the reasoning.

Wiring this collector into gRParse (the `COLLECTOR_*` enum and the endpoint
env) is a follow-up in that repo, not here.

## Docs

- [`AGENTS.md`](AGENTS.md) — read order, definition of done, git
- [`docs/architecture.md`](docs/architecture.md) — where this sits in the collector fleet
- [`docs/design.md`](docs/design.md) — wire API, Document mapping, tests
- [`docs/guidelines.md`](docs/guidelines.md) — fleet rules (streaming, proto, diskless, git)

## Remotes

- **Forgejo** (`git.rokkon.com/ai-pipestream/grpc-epub`) is the source of truth. `main` lives here.
- **GitHub** is a public push-mirror of `main`. Do not merge to GitHub `main`.
- GitHub's default branch is `development` so LLM / `gh` work lands there instead of clobbering the mirror.

Push Forgejo first. GitHub `main` updates from the Forgejo push-mirror.

## License

Apache-2.0. See [LICENSE](LICENSE).
