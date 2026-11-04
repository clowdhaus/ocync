// Rasterizes the two SVG banners to the PNGs the README and the page metadata
// point at. `cargo xtask bench-remote --publish` runs it after it writes the
// SVGs, so a PNG never shows figures its SVG does not.
//
// Usage: node scripts/render-banners.mjs [output-dir]   (default: public/)
//
// An SVG's user units are 72 dpi, so `density` is 72 times the scale from the
// SVG's size to the PNG's.
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";

const publicDir = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "public");
const outDir = process.argv[2] ? path.resolve(process.argv[2]) : publicDir;

const banners = [
  { name: "ecr-banner", width: 1800, height: 500, density: 144, alpha: false },
  { name: "og-image", width: 1200, height: 630, density: 72, alpha: true },
];

await mkdir(outDir, { recursive: true });
for (const { name, width, height, density, alpha } of banners) {
  let image = sharp(path.join(publicDir, `${name}.svg`), { density });
  if (!alpha) {
    image = image.removeAlpha();
  }
  const info = await image.png().toFile(path.join(outDir, `${name}.png`));
  if (info.width !== width || info.height !== height) {
    throw new Error(`${name}.png is ${info.width}x${info.height}, not ${width}x${height}`);
  }
  console.log(`${name}.png ${info.width}x${info.height}`);
}
