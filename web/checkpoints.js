// Checkpoints of a container's memory, kept as pages.
//
// A container's memory is hundreds of megabytes even when the program is
// small: the kernel reserves its guest block at boot, and a wasm memory
// grows and never shrinks. Nearly all of it is zero, and nearly all of what
// is not is still between two checkpoints — the image blob never changes,
// and page ownership keeps a dormant process's pages in place. So a
// checkpoint is a map from 4 KiB page index to the page's bytes, holding
// only pages that are not zero; the pages are immutable and shared between
// checkpoints; and a checkpoint after the first is recorded as the pages
// that changed, found by comparing memory against the previous map word by
// word. Keeping a full map every so often costs only the map itself, since
// its pages are the ones already held, and bounds how many deltas a
// reconstruction applies.
//
// Most of memory is the guest block, and the module keeps a log of which
// of its pages were written (`Container.written`), so a checkpoint taken
// from the container the last one was taken from compares only those and
// the module's own memory around the block, less what the kernel says it
// never writes again (the linked image, decompressed files' buffers).
//
// Restoring never builds a dense image: a fresh wasm memory is zero, so the
// pages are written into it directly.

export const PAGE = 4096;
const WORDS = PAGE >>> 2;

/// The pages of `memory` that differ from `pages` (a page absent from the
/// map is all zero), as a delta: `{ length, changed: Map<page, bytes> }`.
///
/// With `written` — `{ guest, frozen, bitmap }`: `[start, end)` runs of
/// pages only the guest writes, runs nothing writes any more, and a bitmap
/// of the guest pages that may have changed — a guest page the bitmap
/// leaves out, or a page in a frozen run, is taken to be unchanged, and not
/// read.
export function diff(pages, memory, written = null) {
  const words = new Uint32Array(memory.buffer, memory.byteOffset, memory.byteLength >>> 2);
  const count = Math.ceil(memory.length / PAGE);
  const changed = new Map();
  const compare = (page) => {
    const start = page * WORDS;
    const end = Math.min(start + WORDS, words.length);
    const before = pages?.get(page);
    let same = true;
    if (before) {
      const beforeWords = new Uint32Array(before.buffer, before.byteOffset, before.byteLength >>> 2);
      for (let i = start, j = 0; i < end; i++, j++) {
        if (beforeWords[j] !== words[i]) {
          same = false;
          break;
        }
      }
    } else {
      for (let i = start; i < end; i++) {
        if (words[i] !== 0) {
          same = false;
          break;
        }
      }
    }
    if (!same) {
      const from = page * PAGE;
      changed.set(page, memory.slice(from, Math.min(from + PAGE, memory.length)));
    }
  };
  if (!written) {
    for (let page = 0; page < count; page++) compare(page);
    return { length: memory.length, changed };
  }
  // Every page is compared, unless it is the guest's and unwritten, or frozen.
  const skip = new Uint8Array(count);
  for (const [from, to] of [...written.guest, ...written.frozen]) skip.fill(1, Math.min(from, count), Math.min(to, count));
  const bitmap = written.bitmap;
  for (const [from, to] of written.guest) {
    for (let word = from >>> 5; word < bitmap.length && word << 5 < Math.min(to, count); word++) {
      let bits = bitmap[word];
      while (bits) {
        const bit = 31 - Math.clz32(bits);
        bits &= ~(1 << bit);
        const page = (word << 5) + bit;
        if (page >= from && page < to && page < count) skip[page] = 0;
      }
    }
  }
  for (let page = 0; page < count; page++) if (!skip[page]) compare(page);
  return { length: memory.length, changed };
}

/// `pages` with a delta applied: a new map sharing the unchanged pages.
export function apply(pages, delta) {
  const next = new Map(pages);
  for (const [page, bytes] of delta.changed) next.set(page, bytes);
  return next;
}

/// A dense image from a page map, for comparing against a full snapshot.
export function dense(pages, length) {
  const image = new Uint8Array(length);
  for (const [page, bytes] of pages) image.set(bytes.subarray(0, Math.min(bytes.length, length - page * PAGE)), page * PAGE);
  return image;
}

export class Checkpoints {
  constructor({ fullEvery = 16 } = {}) {
    this.fullEvery = fullEvery;
    this.entries = []; // { at, length, stackPointer, mounts, delta, full?: Map }
    this.latest = null; // the page map of the last checkpoint added
    this.materialised = null; // { index, pages }
    this.held = 0; // bytes of page data kept
    this.tracked = null; // { container, guest, frozen }: whose written log `latest` agrees with
    this.spent = 0; // milliseconds spent taking checkpoints
    this.lastCost = undefined; // ms the last one took
    this.estimate = undefined; // ms the next one should take, when known
    this.costs = []; // ms the last few took
  }

  /// Adds a checkpoint taken at `at` from a container standing between
  /// turns. Reads its memory in place; copies only the pages that changed.
  add(at, container) {
    const began = performance.now();
    // The log is good only for the container it was kept by, and only from
    // a checkpoint of that container on: the first one compares everything,
    // and starts the log afresh.
    // A run frozen now may have been written since the last checkpoint, so
    // what is skipped is what was already frozen then.
    const bitmap = container.written();
    let written = null;
    const layout = bitmap && container.pageLayout();
    if (bitmap && this.tracked?.container === container) written = { ...this.tracked, bitmap };
    this.tracked = bitmap ? { container, ...layout } : null;
    // Viewed after asking, since an answer can grow memory.
    const memory = new Uint8Array(container.memory.buffer);
    const delta = diff(this.latest, memory, written);
    for (const bytes of delta.changed.values()) this.held += bytes.length;
    this.latest = apply(this.latest ?? new Map(), delta);
    const entry = { at, length: delta.length, stackPointer: container.stackPointer, mounts: container.mounts.snapshot(), delta };
    if (this.entries.length % this.fullEvery === 0) entry.full = this.latest;
    this.entries.push(entry);
    this.lastCost = performance.now() - began;
    // What the next one should cost: the median of the last few, so that
    // one that copied a burst of fresh pages — a program just loaded — does
    // not hold the next off as if they all would. One that compared
    // everything to start a written log is not counted: the next will not.
    if (!(bitmap && !written)) this.costs = [...this.costs.slice(-4), this.lastCost];
    const sorted = [...this.costs].sort((a, b) => a - b);
    this.estimate = sorted.length ? sorted[sorted.length >> 1] : undefined;
    this.spent += this.lastCost;
    return this.entries.length - 1;
  }

  /// The most instructions a seek before `frontier` re-executes: the widest
  /// gap between checkpoints, the last one's to the frontier included.
  widest(frontier) {
    let widest = 0;
    for (let i = 0; i < this.entries.length; i++) widest = Math.max(widest, (this.entries[i + 1]?.at ?? frontier) - this.entries[i].at);
    return widest;
  }

  /// The checkpoint at or before `at`: its index. Of several at the same
  /// count — an idle container's, taken as its board ran on — the first,
  /// which stands where the container got to it.
  before(at) {
    let found = 0;
    this.entries.forEach((entry, index) => {
      if (entry.at <= at && entry.at > this.entries[found].at) found = index;
    });
    return found;
  }

  /// Forgets checkpoint `index`, neither the first nor the last: the pages
  /// it changed go into the next one's delta, unless the next changed them
  /// again, and a full map it kept is carried on to the next. Every other
  /// checkpoint restores as before.
  drop(index) {
    if (index <= 0 || index >= this.entries.length - 1) return false;
    const gone = this.entries[index];
    const next = this.entries[index + 1];
    if (gone.full && !next.full) next.full = apply(gone.full, next.delta);
    for (const [page, bytes] of gone.delta.changed) {
      if (next.delta.changed.has(page)) this.held -= bytes.length;
      else next.delta.changed.set(page, bytes);
    }
    this.entries.splice(index, 1);
    this.materialised = null;
    return true;
  }

  get length() {
    return this.entries.length;
  }

  at(index) {
    return this.entries[index].at;
  }

  /// What the checkpoints would cost as full copies of memory.
  get naive() {
    return this.entries.reduce((sum, entry) => sum + entry.length, 0);
  }

  /// The page map of checkpoint `index`: the nearest full map at or before
  /// it, with every delta since applied in order. The last answer is kept.
  pages(index) {
    if (this.materialised?.index === index) return this.materialised.pages;
    let base = index;
    while (!this.entries[base].full) base--;
    let pages = this.entries[base].full;
    if (this.materialised && this.materialised.index > base && this.materialised.index < index) {
      base = this.materialised.index;
      pages = this.materialised.pages;
    }
    for (let i = base + 1; i <= index; i++) pages = apply(pages, this.entries[i].delta);
    this.materialised = { index, pages };
    return pages;
  }

  /// What a container is restored from, for checkpoint `index`.
  snapshot(index) {
    const entry = this.entries[index];
    return { pages: this.pages(index), length: entry.length, stackPointer: entry.stackPointer, mounts: entry.mounts };
  }

  /// The memory of checkpoint `index` as one image, for a test.
  memory(index) {
    return dense(this.pages(index), this.entries[index].length);
  }
}
