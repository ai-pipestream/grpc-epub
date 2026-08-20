// SPDX-License-Identifier: Apache-2.0
//
// Thin wrapper around the ai.pipestream.epub.v1 gRPC contract.
//
// The protos are loaded dynamically from ../../proto (the single source of
// truth in this repository) — no generated code is checked in.

import { fileURLToPath } from "node:url";
import path from "node:path";
import grpc from "@grpc/grpc-js";
import protoLoader from "@grpc/proto-loader";

const PROTO_ROOT = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  "..", "..", "..", "proto",
);

const packageDefinition = protoLoader.loadSync(
  path.join(PROTO_ROOT, "ai", "pipestream", "epub", "v1", "epub_service.proto"),
  {
    includeDirs: [PROTO_ROOT],
    keepCase: false,
    longs: Number,
    enums: String,
    defaults: true,
    oneofs: true,
  },
);

const { ai } = grpc.loadPackageDefinition(packageDefinition);
const EpubParseService = ai.pipestream.epub.v1.EpubParseService;

/** Upload chunk size. Any value gives the same events; this one is quick. */
export const CHUNK_BYTES = 64 * 1024;

/** A connected grpc-epub client. */
export class EpubClient {
  /** @param {string} address host:port of the grpc-epub server. */
  constructor(address = process.env.EPUB_ADDR ?? "127.0.0.1:50064") {
    this.stub = new EpubParseService(
      address,
      grpc.credentials.createInsecure(),
    );
  }

  /**
   * Open a ParseEpub call and send the options frame.
   *
   * The caller then writes `{ chunk }` frames as the archive becomes available
   * and calls `.end()`. The server buffers the whole upload before it emits
   * anything — the central directory of a ZIP is its last bytes, so no entry
   * can be located until the upload is complete — but it still reads the
   * request stream rather than a length, so the options-first ordering is the
   * caller's job.
   *
   * @param {object} options a ParseOptions message.
   * @returns {object} the duplex call.
   */
  openParse(options) {
    const call = this.stub.parseEpub();
    call.write({ options });
    return call;
  }

  /**
   * Stream a whole in-memory archive and yield each event as it arrives.
   *
   * @param {Buffer} bytes the .epub archive.
   * @param {object} options a ParseOptions message.
   * @returns {AsyncGenerator<object>} ParseEpubResponse messages.
   */
  async *parse(bytes, options) {
    const call = this.openParse(options);

    for (let at = 0; at < bytes.length; at += CHUNK_BYTES) {
      call.write({ chunk: bytes.subarray(at, at + CHUNK_BYTES) });
    }
    call.end();

    // grpc-js hands events to callbacks; this turns the callback stream into
    // an async iterator without buffering the whole book's worth.
    const queue = [];
    let waiting = null;
    let done = false;
    let failure = null;

    const wake = () => {
      if (waiting) {
        const resolve = waiting;
        waiting = null;
        resolve();
      }
    };
    call.on("data", (event) => { queue.push(event); wake(); });
    call.on("end", () => { done = true; wake(); });
    call.on("error", (err) => { failure = err; done = true; wake(); });

    for (;;) {
      while (queue.length > 0) yield queue.shift();
      if (done) break;
      await new Promise((resolve) => { waiting = resolve; });
    }
    if (failure) throw failure;
  }

  close() {
    grpc.closeClient(this.stub);
  }
}

/**
 * Reduce one response event to what the demo page draws.
 *
 * `chapter` and `resource` carry the entry bytes verbatim, which the page does
 * not need: a chapter becomes its size plus a short text preview, a resource
 * becomes its size. `info` and `status` forward whole, and an arm this client
 * has never heard of forwards whole too, per the contract's ignore-unknown
 * rule.
 *
 * @param {object} response a ParseEpubResponse.
 * @returns {[string, object] | null} the event name and its summary, or null
 *   for a response with no event set.
 */
export function summarizeEvent(response) {
  const kind = response.event;
  if (!kind) return null;
  const payload = response[kind] ?? {};

  if (kind === "chapter") {
    const content = payload.content ?? Buffer.alloc(0);
    return [kind, {
      spineIndex: payload.spineIndex,
      idref: payload.idref,
      href: payload.href,
      mediaType: payload.mediaType,
      linear: payload.linear,
      properties: payload.properties,
      sizeBytes: content.length,
      preview: textPreview(content),
    }];
  }
  if (kind === "resource") {
    return [kind, {
      href: payload.href,
      mediaType: payload.mediaType,
      kind: payload.kind,
      manifestId: payload.manifestId,
      properties: payload.properties,
      sizeBytes: (payload.content ?? Buffer.alloc(0)).length,
    }];
  }
  if (kind === "document") {
    // The fold is a whole-book message the page has no use for; the name is
    // enough to prove the event arrived.
    return [kind, { name: payload.name ?? "" }];
  }
  return [kind, payload];
}

/**
 * A plain-text excerpt of a chapter's XHTML, for display only.
 *
 * This is not parsing and must never grow into it: what the bytes *mean* is
 * the HTML collector's job. The demo strips tags so a chapter row has
 * something legible to show, nothing more.
 */
function textPreview(content, max = 200) {
  const text = content.toString("utf8")
    .replace(/<\?[\s\S]*?\?>/g, " ")
    .replace(/<!--[\s\S]*?-->/g, " ")
    .replace(/<[^>]*>/g, " ")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&apos;|&#39;/g, "'")
    .replace(/&amp;/g, "&") // last, or "&amp;lt;" would double-decode
    .replace(/\s+/g, " ")
    .trim();
  return text.length > max ? `${text.slice(0, max)}…` : text;
}
