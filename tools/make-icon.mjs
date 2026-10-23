// Emit a minimal valid .ico (solid dark square, GF-colored) — no dependencies.
// Structure: ICONDIR(6) + ICONDIRENTRY(16) + BITMAPINFOHEADER(40) + XOR(pixels) + AND(mask).
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

const width = 16;
const height = 16;
const bpp = 32;

// BGRA pixel rows, bottom-up. Colors: dark bg #0b0b0d, light-blue "GF" block hint.
const pixels = Buffer.alloc(width * height * 4);
for (let y = 0; y < height; y++) {
  for (let x = 0; x < width; x++) {
    const row = height - 1 - y; // bottom-up
    const offset = (row * width + x) * 4;
    const inner = x >= 2 && x <= 13 && y >= 3 && y <= 12;
    // B, G, R, Reserved
    pixels[offset] = inner ? 0xfc : 0x0d;
    pixels[offset + 1] = inner ? 0xd3 : 0x0b;
    pixels[offset + 2] = inner ? 0x7d : 0x0b;
    pixels[offset + 3] = 0xff;
  }
}

const andMask = Buffer.alloc(height * Math.ceil(width / 8) * 1); // all opaque

const header = Buffer.alloc(40);
header.writeUInt32LE(40, 0);
header.writeInt32LE(width, 4);
header.writeInt32LE(height * 2, 8); // XOR + AND
header.writeUInt16LE(1, 12);
header.writeUInt16LE(bpp, 14);
header.writeUInt32LE(0, 20);
header.writeUInt32LE(pixels.length + andMask.length, 16);

const image = Buffer.concat([header, pixels, andMask]);

const dir = Buffer.alloc(6);
dir.writeUInt16LE(0, 0);
dir.writeUInt16LE(1, 2);

const entry = Buffer.alloc(16);
entry.writeUInt8(width, 0);
entry.writeUInt8(height, 1);
entry.writeUInt16LE(1, 4);
entry.writeUInt16LE(bpp, 6);
header.writeUInt32LE(0, 20);
entry.writeUInt32LE(8 + 40 + pixels.length + andMask.length, 8); // bytes in resource = header+data
entry.writeUInt32LE(22, 12); // offset: ICONDIR(6) + ICONDIRENTRY(16) = 22

const ico = Buffer.concat([dir, entry, image]);
const out = process.argv[2] || "icons/icon.ico";
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, ico);
console.log("wrote", out, ico.length, "bytes");
