// Memory in its own form: a run of bytes seen as a picture, as hex or as
// text, every byte of it a place to ask about.
//
// A picture is laid out by what scans it out, when something does — a
// display controller's layer (`board/scanned`): where its frame is, a
// line's pitch, its size and format and colour table — and otherwise by a
// width the bytes' count suggests, a byte a pixel, in grey. Any run of
// bytes can be seen any of the three ways; these say which to begin with.

/// The layer that scans out any of `[address, address + size)`, if one does.
export function layerFor(layers, address, size) {
  return (layers ?? []).find((l) => address < l.address + l.pitch * l.height && l.address < address + size) ?? null;
}

/// How a run of bytes at `address` is laid out as a picture: `{ base,
/// pitch, width, height, bytes, format, clut }` — a layer's own, or a
/// width (`asked`, if given) a byte a pixel.
export function shapeOf(layer, address, size, asked = null) {
  if (layer && !asked) return { base: layer.address, pitch: layer.pitch, width: layer.width, height: layer.height, bytes: layer.bytes, format: layer.format, clut: layer.clut };
  const width = asked ?? guessWidth(size);
  return { base: address, pitch: width, width, height: Math.ceil(size / width), bytes: 1, format: "L8", clut: null };
}

/// A width for `size` bytes a byte a pixel: a common one that divides it
/// into something not too far from square, else 64.
export function guessWidth(size) {
  for (const width of [320, 256, 240, 160, 128, 64, 32]) if (size % width === 0 && size / width >= width / 4 && size / width <= width * 2) return width;
  return 64;
}

/// The pixel a byte is, `{ x, y }`, or null if outside the picture.
export function pixelOf(shape, address) {
  const offset = address - shape.base;
  if (offset < 0) return null;
  const y = Math.floor(offset / shape.pitch);
  const x = Math.floor((offset % shape.pitch) / shape.bytes);
  return x < shape.width && y < shape.height ? { x, y } : null;
}

/// The first byte of a pixel.
export function addressOf(shape, x, y) {
  return shape.base + y * shape.pitch + x * shape.bytes;
}

/// A pixel's colour, `[r, g, b]`, from its bytes as the format has them;
/// a byte that is not known is null, and so is the colour.
export function colour(format, bytes, clut) {
  if (bytes.some((b) => b === null || b === undefined)) return null;
  const five = (v) => ((v << 3) | (v >> 2)) & 0xff;
  const six = (v) => ((v << 2) | (v >> 4)) & 0xff;
  switch (format) {
    case "L8":
    case "AL44":
    case "AL88": {
      const index = format === "AL44" ? bytes[0] & 0x0f : bytes[0];
      if (clut) return [clut[3 * index], clut[3 * index + 1], clut[3 * index + 2]];
      return [index, index, index];
    }
    case "RGB565": {
      const v = bytes[0] | (bytes[1] << 8);
      return [five(v >> 11), six((v >> 5) & 0x3f), five(v & 0x1f)];
    }
    case "RGB888":
    case "ARGB8888":
      return [bytes[2], bytes[1], bytes[0]];
    default:
      return [bytes[0], bytes[0], bytes[0]];
  }
}

/// How the bytes are seen to begin with: as text when nearly all of what
/// is known of them is printable, as a picture when something scans them
/// out or there are many, else as hex.
export function firstView(bytes, layer) {
  if (layer) return "picture";
  const known = bytes.filter((b) => b !== null);
  const printable = known.filter((b) => (b >= 0x20 && b < 0x7f) || b === 0x0a || b === 0x0d || b === 0x09 || b === 0).length;
  if (known.length && printable / known.length > 0.95 && known.some((b) => b >= 0x20)) return "text";
  return bytes.length >= 1024 ? "picture" : "hex";
}

/// A palette from `board/scanned`'s base64, as bytes `[r, g, b, …]`.
export function clutBytes(base64) {
  if (!base64) return null;
  const text = atob(base64);
  const out = new Uint8Array(text.length);
  for (let i = 0; i < text.length; i++) out[i] = text.charCodeAt(i);
  return out;
}
