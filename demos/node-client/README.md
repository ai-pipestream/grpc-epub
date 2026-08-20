# Node demo client

A live web viewer for the grpc-epub server. Stubs are loaded dynamically from
[`../../proto`](../../proto) at run time, so nothing generated is checked in.

```bash
npm install

# Web viewer, then open http://127.0.0.1:8086
npm start
```

`npm start` first runs `scripts/make-samples.mjs`, which writes the sample
books into [`../sample-data`](../sample-data). They are generated rather than
committed: this repository keeps no binaries, and the books mirror the
fixtures the Rust test suite authors in memory.

The bridge honours `EPUB_ADDR` (default `127.0.0.1:50064`) and `PORT`
(default `8086`).

### Serving under a base path

Set `UI_BASE` and the whole viewer moves under that prefix, for example behind
a reverse proxy that forwards `/ui/epub/*` unchanged:

```bash
UI_BASE=/ui/epub npm start   # page at http://127.0.0.1:8086/ui/epub/
```

The bridge strips the prefix before routing, so every endpoint lives at
`$UI_BASE/api/*`, and it injects a `<meta name="ui-base">` tag into the served
page, which the page reads to prefix its own `fetch()` calls. Unset, nothing
changes: the bridge answers at the root exactly as before. This is how the
shared demo shell mounts the viewer as its "epub" tab.

## The web viewer

The viewer exists to make two properties visible: **nothing can begin until
the last byte of the upload**, because a ZIP's central directory sits at the
end of the file, and **once it can begin, it begins immediately** — `info`
before any chapter is inflated, then each `chapter` as its entry comes off
the archive.

It is a single HTTP request. The browser POSTs the archive and reads
Server-Sent Events off the *same* response, which is deliberately the same
shape as the gRPC call underneath it. Nothing buffers the book in the bridge:
each upload slice is written into the gRPC call as it lands, and each event is
flushed to the page as the Rust server emits it.

The page shows the upload bar, then the book's metadata and a spine progress
bar that fills chapter by chapter, and reports the milliseconds from the last
uploaded byte to the first chapter — on a local server, single digits.

Two controls make this observable rather than theoretical:

- **Upload throttle** sleeps between upload slices. It slows the *upload*
  only, so the bar's fill is watchable on a small book.
- **Event pace** pauses the gRPC stream between events, so chapters are drawn
  one at a time instead of in a single paint. It is display pacing: the pause
  pushes back through gRPC flow control, and the events themselves are
  unchanged.

Worth trying:

| Book | What you see |
|---|---|
| `long-book.epub` | twelve chapters arriving in spine order, progress bar filling |
| `two-chapters.epub` | cover stored after the chapters, so the `resource` event arrives last |
| `cover-first.epub` | the same book with the cover stored first, so its `resource` event arrives before chapter 0 |

The chapter previews on the page are the bridge stripping tags for display.
What the XHTML *means* remains the HTML collector's job; this service hands
the bytes on verbatim.

### A real book

The generated fixtures are small on purpose. Drop any real `.epub` into
[`../sample-data/large/`](../sample-data) (gitignored) and it appears in the
dropdown, or use the file picker for something on your disk. Project
Gutenberg is the obvious source, and its books exercise the
image-between-chapters ordering the small fixtures can only sketch.

### Why SSE is parsed by hand

`EventSource` only does `GET`, and the whole point is that the upload and the
event stream are one request. So the page reads `response.body` as a stream
and splits frames itself. It is about fifteen lines and it is in `stream()`
in `public/index.html`.

## Things that bite

**Send the options frame first.** The first frame of the request stream must
carry `options`; every frame after it is a `chunk`. `lib/epub.js` writes
options inside `openParse()` so the ordering cannot be got wrong by a caller.

**The upload completes before the first event, and that is the format, not
the server.** Do not "fix" a client for what looks like a stalled stream
mid-upload. The demo's timing stat exists to show how small the real latency
is once the archive is whole.

**Handle the oneof by name, not by guessing.** With `oneofs: true`,
proto-loader sets `message.event` to the name of the active arm. The bridge
forwards `message[message.event]` rather than sniffing which key is
populated, so an arm added to the contract later is passed through to the
page instead of being dropped silently.

**Backpressure is real and worth keeping.** `res.write()` returning false
means the browser is behind. The bridge pauses the gRPC call and resumes on
`drain`, which propagates through gRPC flow control back to the server.
