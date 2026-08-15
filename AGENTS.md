# AGENTS.md: grpc-epub

grpc-epub is implemented in this repo (Rust, tonic; `cargo test` is green).
The specs in `docs/` record the intent; where they and the code disagree on
behavior, the code is right and the spec should be updated.

## Read this first, in order

1. This file
2. `docs/architecture.md`: fleet boundary, language, what we refuse to own
3. `docs/design.md`: wire API sketch, Document mapping, tests
4. `docs/guidelines.md`: fleet rules (streaming, proto, git, tests)

Do not start coding until those four are in your context. If architecture
and an existing sibling disagree on *process* (diskless, health, buf),
follow the sibling. If they disagree on *product* (live stream, Document
plane), follow architecture.md.

## This service

gRPC EPUB collector that unpacks the spine and projects chapters into the gRParse Document data plane

- **Language:** Rust (tonic, zip crate over Cursor<&[u8]>, quick-xml for container/OPF only)
- **Copy from:** /work/main/grpc-services/grpc-calamine and /work/main/grpc-services/grpc-lol-html
- **Stack:** Thin packager. You own ZIP/OPF/spine. HTML semantics belong to the HTML collector: emit Chapter XHTML bytes, do not reimplement BeautifulSoup.
- **Live stream:** EpubInfo (title, spine length) immediately, then each Chapter as that zip entry is read, Resource images, ParseStatus.

## Definition of done (v1)

ParseEpub stream, zip-bomb cap, path-traversal reject, in-test EPUB builder, health+reflection, read-only image. All implemented.

Also: README with build/run; proto lint clean; tests that fail if someone
turns the stream back into a batch (assert an event before the input is
fully consumed, or per-item events before Complete). `tests/streaming.rs`
is that test.

## Workspace

Checkout path: `/work/main/grpc-services/grpc-epub`.
Git: `origin` = Forgejo (push `main` here). `github` = GitHub mirror.
Never merge GitHub `main`. See `docs/guidelines.md`.

gRParse wiring (`COLLECTOR_*` enum, endpoint env) is a **follow-up**.
Ship a working server in this repo first.
