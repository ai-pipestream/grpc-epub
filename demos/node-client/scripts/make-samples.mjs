// SPDX-License-Identifier: Apache-2.0
//
// Writes the demo's sample EPUBs into ../sample-data.
//
// This repository commits no binaries — the Rust test suite authors its
// fixtures in memory, and the demo follows the same rule by *generating* its
// books here instead of checking them in. A ZIP is a simple container, so the
// writer below is a hundred lines of stdlib node rather than a dependency.
//
// The books mirror tests/common/mod.rs: `two-chapters.epub` is the default
// fixture with its cover stored after the chapters, `cover-first.epub` is the
// same book with the cover stored first, and the pair exists to show that a
// resource arrives when its archive entry is reached. `long-book.epub` is
// twelve padded chapters, enough to watch the spine stream.

import { mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { deflateRawSync } from "node:zlib";

const OUT_DIR = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..", "..", "sample-data",
);

// --- CRC-32 (ISO 3309, the ZIP polynomial) ----------------------------------

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(buf) {
  let crc = 0xffffffff;
  for (const byte of buf) {
    crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

// --- ZIP writer: local headers, central directory, end record ---------------

const STORED = 0;
const DEFLATED = 8;

function buildZip(entries) {
  const chunks = [];
  const central = [];
  let offset = 0;

  for (const { name, data, method } of entries) {
    const nameBytes = Buffer.from(name, "utf8");
    const stored = method === STORED ? data : deflateRawSync(data, { level: 9 });
    const crc = crc32(data);

    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0); // local file header signature
    local.writeUInt16LE(20, 4); // version needed
    local.writeUInt16LE(0, 6); // flags
    local.writeUInt16LE(method, 8);
    local.writeUInt16LE(0, 10); // mod time
    local.writeUInt16LE(0x21, 12); // mod date (1980-01-01)
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(stored.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    local.writeUInt16LE(0, 28); // extra length
    chunks.push(local, nameBytes, stored);

    const record = Buffer.alloc(46);
    record.writeUInt32LE(0x02014b50, 0); // central directory signature
    record.writeUInt16LE(20, 4); // version made by
    record.writeUInt16LE(20, 6); // version needed
    record.writeUInt16LE(0, 8); // flags
    record.writeUInt16LE(method, 10);
    record.writeUInt16LE(0, 12); // mod time
    record.writeUInt16LE(0x21, 14); // mod date
    record.writeUInt32LE(crc, 16);
    record.writeUInt32LE(stored.length, 20);
    record.writeUInt32LE(data.length, 24);
    record.writeUInt16LE(nameBytes.length, 28);
    // extra, comment, disk, internal attrs, external attrs: all zero
    record.writeUInt32LE(offset, 42); // local header offset
    central.push(record, nameBytes);

    offset += local.length + nameBytes.length + stored.length;
  }

  const directory = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0); // end of central directory signature
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);

  return Buffer.concat([...chunks, directory, end]);
}

// --- The books, mirroring tests/common/mod.rs --------------------------------

const OPF_PATH = "OEBPS/content.opf";
const CHAP1 = "OEBPS/text/chap1.xhtml";
const CHAP2 = "OEBPS/text/chap2.xhtml";
const COVER = "OEBPS/images/cover.png";

// A real PNG signature followed by filler, same as the test fixture: nothing
// in the service decodes an image, so a decodable PNG would demonstrate
// nothing extra.
const IMAGE = Buffer.from("\x89PNG\r\n\x1a\ncover-image-bytes-for-the-round-trip-assertion", "latin1");

const containerXml = `<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="${OPF_PATH}" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>`;

function opfXml(spine, resources) {
  const manifest = [
    ...spine.map(([id, href]) =>
      `    <item id="${id}" href="${href}" media-type="application/xhtml+xml"/>`),
    ...resources.map(([id, href, mediaType, properties]) =>
      `    <item id="${id}" href="${href}" media-type="${mediaType}"` +
      `${properties ? ` properties="${properties}"` : ""}/>`),
  ].join("\n");
  const itemrefs = spine.map(([id]) => `    <itemref idref="${id}"/>`).join("\n");
  return `<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>A Tale of Two Chapters</dc:title>
    <dc:creator>Ada Lovelace</dc:creator>
    <dc:creator>Charles Babbage</dc:creator>
    <dc:language>en-GB</dc:language>
    <dc:identifier id="bookid" opf:scheme="ISBN">urn:isbn:9780000000000</dc:identifier>
    <dc:publisher>Analytical Press</dc:publisher>
    <dc:date>1843-10-01</dc:date>
    <dc:subject>Computing</dc:subject>
  </metadata>
  <manifest>
${manifest}
  </manifest>
  <spine>
${itemrefs}
  </spine>
</package>`;
}

function chapterXhtml(heading, body) {
  return `<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>${heading}</title></head>
<body><h1>${heading}</h1><p>${body}</p><img src="../images/cover.png" alt="cover"/></body></html>`;
}

/** `mimetype` stored and first, as the EPUB specification requires. */
function shell() {
  return [
    { name: "mimetype", data: Buffer.from("application/epub+zip"), method: STORED },
    { name: "META-INF/container.xml", data: Buffer.from(containerXml), method: DEFLATED },
  ];
}

const deflate = (name, text) => ({ name, data: Buffer.from(text), method: DEFLATED });

const TWO_CHAPTER_OPF = opfXml(
  [["ch1", "text/chap1.xhtml"], ["ch2", "text/chap2.xhtml"]],
  [["cover-img", "images/cover.png", "image/png", "cover-image"]],
);

/** The default two-chapter book: cover image stored *after* the chapters. */
function twoChapters() {
  return buildZip([
    ...shell(),
    deflate(OPF_PATH, TWO_CHAPTER_OPF),
    deflate(CHAP1, chapterXhtml("Chapter One", "The first chapter.")),
    deflate(CHAP2, chapterXhtml("Chapter Two", "The second chapter.")),
    { name: COVER, data: IMAGE, method: DEFLATED },
  ]);
}

/** The same book with the cover stored *before* the chapters. */
function coverFirst() {
  return buildZip([
    ...shell(),
    deflate(OPF_PATH, TWO_CHAPTER_OPF),
    { name: COVER, data: IMAGE, method: DEFLATED },
    deflate(CHAP1, chapterXhtml("Chapter One", "The first chapter.")),
    deflate(CHAP2, chapterXhtml("Chapter Two", "The second chapter.")),
  ]);
}

/** Twelve chapters of a few KiB each, so the spine visibly streams. */
function longBook() {
  const count = 12;
  const spine = Array.from({ length: count }, (_, i) => [`ch${i}`, `text/chap${i}.xhtml`]);
  const entries = [
    ...shell(),
    deflate(OPF_PATH, opfXml(spine, [])),
  ];
  for (let i = 0; i < count; i++) {
    const filler = `Chapter ${i} body. `.repeat(400);
    entries.push(deflate(`OEBPS/text/chap${i}.xhtml`, chapterXhtml(`Chapter ${i}`, filler)));
  }
  return buildZip(entries);
}

mkdirSync(OUT_DIR, { recursive: true });
for (const [name, build] of [
  ["two-chapters.epub", twoChapters],
  ["cover-first.epub", coverFirst],
  ["long-book.epub", longBook],
]) {
  const bytes = build();
  const file = path.join(OUT_DIR, name);
  writeFileSync(file, bytes);
  console.log(`wrote ${path.relative(process.cwd(), file)} (${bytes.length} bytes)`);
}
