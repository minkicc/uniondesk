// Generates the application icons from a tiny built-in drawing script.
//
// Keeping the generator in the repository means the icons can be regenerated
// without a graphics toolchain, and `cargo tauri icon` can still be used later
// to derive the full platform set from icons/icon.png.
//
// Usage: node tools/make-icons.mjs

import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// Resolved from the working directory so the script can also be piped in with
// `node --input-type=module -e`, which avoids Node resolving this file's path.
const root = process.env.UNIONDESK_ROOT ?? process.cwd();
const outDir = join(root, "apps", "desktop", "src-tauri", "icons");
mkdirSync(outDir, { recursive: true });

const ACCENT = [0x2f, 0x6f, 0xed, 0xff];
const ACCENT_DARK = [0x1b, 0x43, 0x9b, 0xff];
const PAPER = [0xf7, 0xf9, 0xff, 0xff];
const TRANSPARENT = [0, 0, 0, 0];

/// Draws the UnionDesk mark: two rounded panels joined by a link, which reads as
/// "two machines, one set of controls".
function draw(size) {
  const pixels = new Uint8Array(size * size * 4);
  const put = (x, y, colour) => {
    if (x < 0 || y < 0 || x >= size || y >= size) return;
    const offset = (y * size + x) * 4;
    pixels[offset] = colour[0];
    pixels[offset + 1] = colour[1];
    pixels[offset + 2] = colour[2];
    pixels[offset + 3] = colour[3];
  };
  const roundedRect = (x0, y0, x1, y1, radius, colour) => {
    for (let y = y0; y < y1; y++) {
      for (let x = x0; x < x1; x++) {
        const dx = Math.max(x0 + radius - x, x - (x1 - 1 - radius), 0);
        const dy = Math.max(y0 + radius - y, y - (y1 - 1 - radius), 0);
        if (dx * dx + dy * dy <= radius * radius) put(x, y, colour);
      }
    }
  };

  const pad = Math.round(size * 0.09);
  const gap = Math.max(1, Math.round(size * 0.06));
  const radius = Math.max(1, Math.round(size * 0.13));
  const half = Math.floor((size - pad * 2 - gap) / 2);
  const screenHeight = Math.round(size * 0.44);
  const top = Math.round((size - screenHeight) / 2) - Math.round(size * 0.06);

  // Backdrop.
  roundedRect(0, 0, size, size, Math.round(size * 0.22), TRANSPARENT);
  roundedRect(pad, top, pad + half, top + screenHeight, radius, ACCENT);
  roundedRect(
    pad + half + gap,
    top,
    size - pad,
    top + screenHeight,
    radius,
    ACCENT_DARK,
  );

  // Link between the two panels.
  const linkY = top + screenHeight;
  const linkWidth = Math.max(1, Math.round(size * 0.09));
  roundedRect(
    Math.round(size / 2 - linkWidth / 2),
    linkY - Math.round(size * 0.02),
    Math.round(size / 2 + linkWidth / 2),
    linkY + Math.round(size * 0.1),
    Math.max(1, Math.round(linkWidth / 2)),
    PAPER,
  );

  // Stand.
  const standWidth = Math.round(size * 0.26);
  roundedRect(
    Math.round(size / 2 - standWidth / 2),
    linkY + Math.round(size * 0.08),
    Math.round(size / 2 + standWidth / 2),
    size - pad,
    Math.max(1, Math.round(size * 0.03)),
    PAPER,
  );
  return pixels;
}

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) {
      crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}

function toPng(size) {
  const pixels = draw(size);
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0; // filter: none
    Buffer.from(pixels.buffer, y * size * 4, size * 4).copy(
      raw,
      y * (size * 4 + 1) + 1,
    );
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/// A 32x32 32-bit BMP based .ico, which every Windows version understands.
function toIco(size) {
  const pixels = draw(size);
  const header = Buffer.alloc(40);
  header.writeUInt32LE(40, 0);
  header.writeInt32LE(size, 4);
  header.writeInt32LE(size * 2, 8); // XOR + AND mask
  header.writeUInt16LE(1, 12);
  header.writeUInt16LE(32, 14);
  const image = Buffer.alloc(size * size * 4);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const source = ((size - 1 - y) * size + x) * 4; // bottom-up
      const target = (y * size + x) * 4;
      image[target] = pixels[source + 2];
      image[target + 1] = pixels[source + 1];
      image[target + 2] = pixels[source];
      image[target + 3] = pixels[source + 3];
    }
  }
  const mask = Buffer.alloc((size * size) / 8);
  const body = Buffer.concat([header, image, mask]);
  const directory = Buffer.alloc(6 + 16);
  directory.writeUInt16LE(0, 0);
  directory.writeUInt16LE(1, 2);
  directory.writeUInt16LE(1, 4);
  directory.writeUInt8(size === 256 ? 0 : size, 6);
  directory.writeUInt8(size === 256 ? 0 : size, 7);
  directory.writeUInt8(0, 8);
  directory.writeUInt8(0, 9);
  directory.writeUInt16LE(1, 10);
  directory.writeUInt16LE(32, 12);
  directory.writeUInt32LE(body.length, 14);
  directory.writeUInt32LE(directory.length, 18);
  return Buffer.concat([directory, body]);
}

/// An .icns container. Modern macOS accepts PNG payloads, so the icon set is
/// simply the same artwork at the sizes Apple's conventions expect.
function toIcns() {
  const variants = [
    ["ic11", 32],
    ["ic12", 64],
    ["ic07", 128],
    ["ic13", 256],
    ["ic08", 256],
    ["ic14", 512],
    ["ic09", 512],
    ["ic10", 1024],
  ];
  const chunks = variants.map(([type, size]) => {
    const png = toPng(size);
    const header = Buffer.alloc(8);
    header.write(type, 0, "ascii");
    header.writeUInt32BE(png.length + 8, 4);
    return Buffer.concat([header, png]);
  });
  const body = Buffer.concat(chunks);
  const header = Buffer.alloc(8);
  header.write("icns", 0, "ascii");
  header.writeUInt32BE(body.length + 8, 4);
  return Buffer.concat([header, body]);
}

writeFileSync(join(outDir, "32x32.png"), toPng(32));
writeFileSync(join(outDir, "128x128.png"), toPng(128));
writeFileSync(join(outDir, "128x128@2x.png"), toPng(256));
writeFileSync(join(outDir, "icon.png"), toPng(512));
writeFileSync(join(outDir, "icon.ico"), toIco(64));
writeFileSync(join(outDir, "icon.icns"), toIcns());
console.log(`icons written to ${outDir}`);
