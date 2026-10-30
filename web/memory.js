// The board's memory, drawn: each RAM a picture whose cells are laid on a
// Hilbert curve, so a power-of-two region — what Hubris gives each task,
// for the MPU — is a square or a 2:1 block of its own; each cell coloured
// by whose it is, and lit by how lately it was written, fading in the
// board's own time. Under it, each task's stack and how deep it has been,
// and an inspector (`inspector.js`): what is chosen, as its DWARF says it
// is — typed statics, the stack frame by frame, the source lines that
// wrote it — each part lit the same way, with the time of its last write a
// handle on the instant.
//
// What it reads, from the board's store: `board/writes/{since}/{cell}` for
// the cells written since the last read, `board/tasks` and
// `board/symbols/{owner}` for what the memory is, `board/memory` and
// `board/written` for the bytes of what the inspector shows, and
// `board/whatis` for what the pointer is on.

import { Inspector } from "./inspector.js";
import { search, shortFile } from "./symbols.js";

/// The bytes a cell of the picture stands for.
export const CELL = 64;

/// Where the `d`th point of a Hilbert curve over a `side`×`side` square
/// (`side` a power of two) is.
export function hilbert(side, d) {
  let x = 0;
  let y = 0;
  for (let s = 1; s < side; s *= 2) {
    const rx = 1 & (d >> 1);
    const ry = 1 & (d ^ rx);
    if (ry === 0) {
      if (rx === 1) {
        x = s - 1 - x;
        y = s - 1 - y;
      }
      [x, y] = [y, x];
    }
    x += s * rx;
    y += s * ry;
    d >>= 2;
  }
  return [x, y];
}

/// A RAM of `cells` cells as tiles side by side, largest first: each a
/// power of two of them, a Hilbert square or two squares across — 288 KiB
/// is a 256 KiB square and a 32 KiB pair. Each tile: its first cell, how
/// many, where it starts across, and its squares' side.
export function tiles(cells) {
  const out = [];
  let start = 0;
  let x = 0;
  for (let bit = 30; bit >= 0; bit--) {
    const count = 2 ** bit;
    if (cells - start < count) continue;
    const side = 2 ** Math.floor(bit / 2);
    const across = bit % 2 ? 2 * side : side;
    out.push({ start, count, x, side, width: across, height: side });
    start += count;
    x += across;
  }
  return out;
}

/// A RAM's picture: each cell's place, and each place's cell.
export function layout(length, cell = CELL) {
  const cells = Math.ceil(length / cell);
  const parts = tiles(cells);
  const width = parts.reduce((w, t) => w + t.width, 0);
  const height = Math.max(1, ...parts.map((t) => t.height));
  const place = new Int32Array(2 * cells);
  const at = new Int32Array(width * height).fill(-1);
  for (const t of parts) {
    const square = t.side * t.side;
    for (let k = 0; k < t.count; k++) {
      let [x, y] = hilbert(t.side, k % square);
      x += t.x + (k >= square ? t.side : 0);
      const n = t.start + k;
      place[2 * n] = x;
      place[2 * n + 1] = y;
      at[y * width + x] = n;
    }
  }
  return { cells, cell, width, height, place, at };
}

/// The board's owners of memory, from `board/tasks`: the kernel and each
/// task, with its colour and the ranges of RAM it has — its stack, and
/// each section it writes — each with what it is.
export function owners(tasks, colorOf) {
  if (!tasks?.kernel) return [];
  return [tasks.kernel, ...tasks.tasks].map((o) => {
    const ranges = o.sections.filter((s) => s.writable).map((s) => ({ base: s.base, end: s.base + s.size, part: s.name }));
    if (o.stack) ranges.push({ base: o.stack.base, end: o.stack.base + o.stack.size, part: "stack" });
    ranges.sort((a, b) => a.base - b.base);
    return { name: o.name, index: o.index, color: o.index === null ? "hsl(210 8% 62%)" : colorOf(o.index), stack: o.stack, ranges };
  });
}

/// Whose each cell of a RAM at `base` is — the owner's number in `list`,
/// or -1 — and whether it is a stack (1) or not (0): the owner of the
/// most of its bytes.
export function cellOwners(base, view, list) {
  const owner = new Int16Array(view.cells).fill(-1);
  const stack = new Uint8Array(view.cells);
  const most = new Int32Array(view.cells);
  list.forEach((o, n) => {
    for (const r of o.ranges) {
      const from = Math.max(r.base, base);
      const to = Math.min(r.end, base + view.cells * view.cell);
      for (let a = from; a < to; ) {
        const c = Math.floor((a - base) / view.cell);
        const cellEnd = base + (c + 1) * view.cell;
        const bytes = Math.min(cellEnd, to) - a;
        if (bytes > most[c]) {
          most[c] = bytes;
          owner[c] = n;
          stack[c] = r.part === "stack" ? 1 : 0;
        }
        a = cellEnd;
      }
    }
  });
  return { owner, stack };
}

/// How hot a write `age` microseconds old is, with heat halving every
/// `half` microseconds: 1 just written, towards 0.
export function heat(age, half) {
  return age <= 0 ? 1 : Math.pow(0.5, age / half);
}

/// The colour of heat `h` in [0, 1]: from a dark red through orange and
/// yellow to white.
export function hot(h) {
  const stops = [
    [0, [120, 20, 30]],
    [0.35, [220, 70, 20]],
    [0.7, [255, 190, 40]],
    [1, [255, 255, 230]],
  ];
  for (let i = 1; i < stops.length; i++) {
    const [p, b] = stops[i];
    const [q, a] = stops[i - 1];
    if (h <= p) {
      const f = (h - q) / (p - q);
      return a.map((v, j) => Math.round(v + (b[j] - v) * f));
    }
  }
  return stops.at(-1)[1];
}

/// Heat laid on a picture of memory: `points` — `{ addr, length, weight,
/// name }`, `weight` over the run's bytes evenly — summed into the cells of
/// `[base, base + length)` at `cell` bytes each. `{ weights, most, outside
/// }`: each cell's, the most any has, and the weight of the points outside
/// the picture by name.
export function fieldCells(base, length, cell, points) {
  const weights = new Float32Array(Math.ceil(length / cell));
  let most = 0;
  const outside = new Map();
  for (const { addr, length: n, weight, name } of points) {
    const from = Math.max(addr, base);
    const to = Math.min(addr + n, base + length);
    if (from >= to) {
      outside.set(name, (outside.get(name) ?? 0) + weight);
      continue;
    }
    const each = weight / n;
    for (let c = Math.floor((from - base) / cell); c * cell + base < to; c++) {
      const lo = Math.max(from, base + c * cell);
      const hi = Math.min(to, base + (c + 1) * cell);
      weights[c] += each * (hi - lo);
      most = Math.max(most, weights[c]);
    }
  }
  return { weights, most, outside };
}

/// How hot a cell of weight `w` is, with `most` the hottest's: on a scale
/// of logarithms, so that one value and a thousand both show; 0 for none.
export function fieldHeat(w, most) {
  if (!(w > 0) || !(most > 0)) return 0;
  return 0.2 + (0.8 * Math.log1p(w)) / Math.log1p(most);
}

/// `hsl(h s% l%)` as [r, g, b].
export function rgbOf(color) {
  const m = /hsl\(\s*([\d.]+)\s+([\d.]+)%\s+([\d.]+)%/.exec(color);
  if (!m) return [128, 128, 128];
  const [h, s, l] = [Number(m[1]), Number(m[2]) / 100, Number(m[3]) / 100];
  const k = (n) => (n + h / 30) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n) => l - a * Math.max(-1, Math.min(k(n) - 3, Math.min(9 - k(n), 1)));
  return [f(0), f(8), f(4)].map((v) => Math.round(v * 255));
}

/// A time in microseconds, as an age: `3.2 ms`.
export function age(us) {
  if (us < 1000) return `${Math.round(us)} µs`;
  if (us < 1e6) return `${+(us / 1000).toFixed(1)} ms`;
  return `${+(us / 1e6).toFixed(2)} s`;
}

/// The aligned block of memory `[base, base + length)` rounds out to: the
/// smallest power-of-two block, aligned to its size, that holds it — one
/// Hilbert square, or two, in a RAM's picture — so long as that stays
/// within the RAM `within`; or, failing that, the run as it is.
export function rounded(base, length, within = null) {
  const fits = (b, n) => !within || (b >= within.base && b + n <= within.base + within.length);
  let n = 4;
  while (n < length) n *= 2;
  for (; n <= 2 ** 31; n *= 2) {
    const aligned = base - (base % n);
    if (aligned + n >= base + length) return fits(aligned, n) ? { base: aligned, length: n } : { base, length };
  }
  return { base, length };
}

/// `rect` of a RAM's picture at `cell` bytes a cell, at the finest grain
/// that draws it with at most `most` cells: a cell of `n` bytes is a
/// square of four of `n / 4` in its place, so each step finer doubles the
/// rectangle's sides, down to a word a cell; and if it is already too
/// many, coarser, up to `coarsest`, halving them outwards.
export function refine(rect, cell, most, coarsest = CELL) {
  while (cell > 4 && rect.w * rect.h * 4 <= most) {
    cell /= 4;
    rect = { x: rect.x * 2, y: rect.y * 2, w: rect.w * 2, h: rect.h * 2 };
  }
  while (rect.w * rect.h > most && cell < coarsest) {
    ({ rect, cell } = coarser(rect, cell));
  }
  return { rect, cell };
}

/// `rect` at the next grain coarser: four times the bytes a cell, its
/// sides halved, outwards.
export function coarser(rect, cell) {
  const x = Math.floor(rect.x / 2);
  const y = Math.floor(rect.y / 2);
  return { cell: cell * 4, rect: { x, y, w: Math.ceil((rect.x + rect.w) / 2) - x, h: Math.ceil((rect.y + rect.h) / 2) - y } };
}

/// The cells of `view` in `rect`: the first and last by address, and how
/// many there are — `null` when there are none.
export function runOf(view, rect) {
  let first = Infinity;
  let last = -1;
  let count = 0;
  for (let y = rect.y; y < rect.y + rect.h; y++)
    for (let x = rect.x; x < rect.x + rect.w; x++) {
      const c = view.at[y * view.width + x];
      if (c < 0) continue;
      first = Math.min(first, c);
      last = Math.max(last, c);
      count++;
    }
  return last < 0 ? null : { first, last, count };
}

/// The rectangle around the cells `[from, to)` of `view`.
export function rectAround(view, from, to) {
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -1;
  let y1 = -1;
  for (let c = from; c < to; c++) {
    const x = view.place[2 * c];
    const y = view.place[2 * c + 1];
    x0 = Math.min(x0, x);
    y0 = Math.min(y0, y);
    x1 = Math.max(x1, x);
    y1 = Math.max(y1, y);
  }
  return { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 };
}

/// Where `rect`, of a RAM's picture at `cell` bytes a cell, falls on a
/// box `width`×`height` showing `shown` of the same RAM's picture at
/// `shownCell` bytes a cell — a cell's side halving as its bytes quarter
/// — clipped to the box, in the box's units; `null` if it misses it.
export function rectOnBox(rect, cell, shown, shownCell, width, height) {
  const f = Math.sqrt(cell / shownCell);
  const px = width / shown.w;
  const py = height / shown.h;
  const left = Math.max(0, (rect.x * f - shown.x) * px);
  const top = Math.max(0, (rect.y * f - shown.y) * py);
  const right = Math.min(width, ((rect.x + rect.w) * f - shown.x) * px);
  const bottom = Math.min(height, ((rect.y + rect.h) * f - shown.y) * py);
  if (right <= left || bottom <= top) return null;
  return { left, top, width: right - left, height: bottom - top };
}

/// Statics as the runs of memory to read for them: those within `gap`
/// bytes of each other read together, each run from a word's start to a
/// word's end, at most `most` bytes in all — what is past that left out.
export function clusters(statics, gap = 256, most = Infinity) {
  const sorted = [...statics].filter((s) => s.size > 0).sort((a, b) => a.base - b.base);
  const out = [];
  for (const s of sorted) {
    const base = s.base - (s.base % 4);
    const end = Math.ceil((s.base + s.size) / 4) * 4;
    const last = out.at(-1);
    if (last && base <= last.end + gap) last.end = Math.max(last.end, end);
    else out.push({ base, end });
  }
  let left = most;
  const kept = [];
  for (const run of out) {
    if (left <= 0) break;
    const end = Math.min(run.end, run.base + left);
    kept.push({ base: run.base, end });
    left -= end - run.base;
  }
  return kept;
}

/// A number of bytes, as `8 KiB` or `96 B`.
export function size(bytes) {
  if (bytes >= 1024 * 1024) return `${+(bytes / 1048576).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${+(bytes / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

/// A static's name, shortened to its last two parts of path, as a row has
/// room for: `drv_stm32xx_i2c_server::__RINGBUF` stays, a generic's
/// `<...>` folded.
export function shortName(name) {
  const folded = name.replace(/<[^<>]*(<[^<>]*>[^<>]*)*>/g, "<…>");
  const parts = folded.split("::");
  return parts.length > 2 ? parts.slice(-2).join("::") : folded;
}

const HALVES = [
  [1e4, "10 ms"],
  [1e5, "100 ms"],
  [1e6, "1 s"],
  [1e7, "10 s"],
];
const SCALE = 3; // pixels a cell
const POLL_MS = 200;
const TASKS_MS = 1000;
const ZOOM_CELLS = 65536; // the most cells a zoomed picture is drawn with
const SERVED = 4096; // bytes a read of memory answers with
const INSPECTED = 65536; // the most of an owner's statics the inspector reads
const BACKGROUND = [22, 25, 30];
/// Where the inspector is, on the pictures.
const HERE = "#4cc2ff";
/// How many places it was before are ringed.
const VISITS = 6;
const AROUND = 256; // bytes shown around an address nothing describes

export class MemoryView {
  /// `read(path)` reads the board's store at the instant in view;
  /// `stand(ps)` stands every panel at an instant of the board's.
  /// `query(options, path)` reads a path from a re-execution to the
  /// instant in view, watching or following on the way (see `inspector.js`).
  constructor(element, { read, query, stand, escape, colorOf, update }) {
    if (update) shown = update;
    this.element = element;
    this.read = read;
    this.query = query;
    this.stand = stand;
    this.escape = escape;
    this.colorOf = colorOf;
    this.half = 1e5;
    this.mark = null;
    this.selected = null; // an owner's name
    this.focus = null; // an address the inspector shows
    this.visits = []; // the addresses shown before it, the latest last: where it was
    this.highlight = null; // runs of memory outlined in the pictures: [address, length]
    this.arrows = []; // pointers drawn in the pictures: [from, to]
    this.field = null; // heat laid on the pictures: { label, back, points }, see `setField`
    this.fieldShown = true;
    this.zooms = []; // the way down: each a rectangle of a RAM's picture, and what it reads
    this.layouts = new Map(); // pictures' layouts, by RAM length and cell
    // Bumped whenever what is shown is changed under the reads made for
    // it — another image, instant or zoom — so an answer to an older one is
    // dropped rather than drawn.
    this.generation = 0;
    this.inspecting = 0; // the same, for the inspector's own reads
    this.latest = 0; // the latest stamp any picture has
    this.reset();
    this.image = null;
    this.owners = [];
    element.innerHTML = `<div class="mem-head"><span class="controls mem-halves"></span> <span class="controls"><button type="button" data-mark title="mark this instant: the cells written since are outlined, and the inspector can show only what was">mark</button></span> <span class="dim mem-note"></span> <span class="mem-symbol"><input type="search" data-symbol placeholder="go to a symbol…" autocomplete="off" spellcheck="false" title="a function, static, type, constant or macro of the board's code, by name or path (Task::new): its source"><span class="sym-results" hidden></span></span><div class="mem-field"></div></div>
      <div class="mem-crumbs"></div>
      <div class="mem-maps"></div>
      <div class="mem-readout dim">&nbsp;</div>
      <div class="mem-owners"></div>
      <div class="mem-inspector"></div>`;
    element.onclick = (event) => this.clicked(event);
    this.whatis = new Map();
    this.inspector = new Inspector(this, element.querySelector(".mem-inspector"));
    this.equipSearch(element.querySelector("[data-symbol]"), element.querySelector(".sym-results"));
    this.inspectedMost = INSPECTED;
  }

  reset() {
    this.rams = null;
    this.detail = null;
    this.now = 0;
    this.latest = 0;
    this.instant = undefined;
    this.inspected = null;
    this.generation++;
  }

  /// What the board's archive says its memory is: read when the image
  /// changes, and again now and then — stacks deepen as the board runs,
  /// and a board whose archive could not be read may yet have one.
  readTasks() {
    const image = this.image;
    this.tasksRead = performance.now();
    this.read("board/tasks")
      .then((tasks) => {
        if (image !== this.image || !tasks) return;
        if (tasks.error !== undefined && tasks.kernel === undefined) {
          this.tasks = { error: tasks.error };
          return this.drawOwners();
        }
        const first = !this.tasks?.kernel;
        this.tasks = tasks;
        if (first) {
          this.owners = owners(tasks, this.colorOf);
          for (const picture of [...(this.rams ?? []), this.detail].filter(Boolean)) Object.assign(picture, cellOwners(picture.base, picture.view, this.owners));
          this.repaint = true;
          this.draw();
        } else this.drawOwners();
        // What each task is doing, from the kernel's own table.
        this.inspector.readSchedules().then(() => {
          if (image === this.image) this.drawOwners();
        });
      })
      .catch(() => {});
  }

  /// The board as the board panel last read it, and the instant the page
  /// stands at (`null`: live).
  update(board, standing) {
    const instant = standing === null ? null : board.time_ps;
    if (board.image !== this.image) {
      this.image = board.image;
      this.reset();
      this.zooms = [];
      this.owners = [];
      this.symbols = new Map();
      this.whatis.clear();
      this.inspector.reset();
      this.highlight = null;
      this.tasks = null;
      this.drawOwners();
      this.readTasks();
    } else if (this.tasks && (instant !== this.instant || performance.now() - (this.tasksRead ?? 0) > TASKS_MS)) this.readTasks();
    if (instant !== this.instant) {
      const image = this.image;
      this.reset();
      this.image = image;
      this.instant = instant;
      // A poll for the instant before may be under way: it is dropped when
      // it answers, and this one made once it has.
      if (this.busy) this.again = true;
      this.poll(true);
    } else this.poll(false);
  }

  /// A picture of a RAM — `board/writes`'s — at `cell` bytes a cell,
  /// showing `rect` of its grid, all of it if none.
  picture(ram, cell, rect = null) {
    const view = this.layoutOf(ram, cell);
    rect ??= { x: 0, y: 0, w: view.width, h: view.height };
    return { ...ram, cell, view, rect, stamps: new Float64Array(view.cells), ...cellOwners(ram.base, view, this.owners), statics: null };
  }

  async poll(force) {
    const now = performance.now();
    if (this.busy || (!force && now - (this.polled ?? 0) < POLL_MS)) return;
    this.busy = true;
    this.polled = now;
    const generation = this.generation;
    try {
      const since = this.rams ? this.now : 0;
      const zoom = this.zooms.at(-1);
      const [answer, detail] = await Promise.all([
        this.read(`board/writes/${since}/${CELL}`),
        zoom ? this.read(`board/writes/${this.detail ? this.now : 0}/${zoom.cell}/${zoom.base}/${zoom.length}`) : null,
      ]);
      // What was asked for is no longer what is shown: dropped, and asked
      // again for what is.
      if (generation !== this.generation) return (this.again = true);
      if (!answer?.rams) return;
      // The board went back — a reset, a seek: everything again, once this
      // poll is done.
      if (this.rams && answer.now < this.now) {
        this.rams = null;
        this.detail = null;
        this.latest = 0;
        return (this.again = true);
      }
      if (!this.rams) this.rams = answer.rams.map((ram) => this.picture(ram, answer.cell));
      const cells = answer.cells;
      for (let i = 0; i < cells.length; i += 3) {
        this.rams[cells[i]].stamps[cells[i + 1]] = cells[i + 2];
        this.latest = Math.max(this.latest, cells[i + 2]);
      }
      let changed = cells.length > 0;
      // Its cells are numbered from the start of what was read, which is
      // cell `first` of the RAM's — at the cell size asked for, which the
      // store keeps to unless the run is too long for it.
      if (zoom && detail?.cells && detail.cell === zoom.cell) {
        if (!this.detail) {
          this.detail = this.picture(this.rams[zoom.ram], zoom.cell, zoom.rect);
          this.nameStatics(this.detail);
          changed = true;
        }
        for (let i = 0; i < detail.cells.length; i += 3) this.detail.stamps[zoom.first + detail.cells[i + 1]] = detail.cells[i + 2];
        changed ||= detail.cells.length > 0;
      }
      this.now = answer.now;
      // Repainted when something was written, or is still fading, or the
      // page asked: an idle board's pictures are left as they are.
      if (changed || this.repaint || heat(this.now - this.latest, this.half) > 0.02) {
        this.repaint = false;
        this.drawMaps();
      }
      this.drawHead();
      this.drawCrumbs();
      this.drawOwners();
      await this.inspect();
    } catch {
      // Nothing to read at this instant.
    } finally {
      this.busy = false;
      // Zoomed while that was read: read for the new picture now.
      if (this.again) {
        this.again = false;
        this.poll(true);
      }
    }
  }

  /// Which static each cell of a zoomed picture is mostly in, once the
  /// statics of the owners it shows are known: their outlines and names,
  /// drawn on it.
  async nameStatics(picture) {
    const generation = this.generation;
    const shown = new Set();
    this.eachCell(picture, (c) => picture.owner[c] >= 0 && shown.add(picture.owner[c]));
    const all = [];
    for (const n of shown) {
      const owner = this.owners[n];
      if (!this.symbols.has(owner.name)) {
        const answer = await this.read(`board/symbols/${owner.name}`).catch(() => null);
        this.symbols.set(owner.name, answer?.statics ?? []);
      }
      all.push(...this.symbols.get(owner.name));
    }
    if (generation !== this.generation) return;
    const statics = new Int32Array(picture.view.cells).fill(-1);
    const most = new Int32Array(picture.view.cells);
    all.forEach((s, i) => {
      const from = Math.max(s.base, picture.base);
      const to = Math.min(s.base + s.size, picture.base + picture.length);
      for (let a = from; a < to; ) {
        const c = Math.floor((a - picture.base) / picture.cell);
        const end = picture.base + (c + 1) * picture.cell;
        const bytes = Math.min(end, to) - a;
        if (bytes > most[c]) {
          most[c] = bytes;
          statics[c] = i;
        }
        a = end;
      }
    });
    picture.statics = statics;
    picture.names = all.map((s) => shortName(s.name));
    picture.fullNames = all.map((s) => s.name);
    if (picture === this.detail) this.drawMaps();
  }

  /// `f(cell, x, y)` for each cell in a picture's rectangle, `x` and `y`
  /// from its corner.
  eachCell(picture, f) {
    const { view, rect } = picture;
    for (let y = 0; y < rect.h; y++)
      for (let x = 0; x < rect.w; x++) {
        const c = view.at[(rect.y + y) * view.width + rect.x + x];
        if (c >= 0) f(c, x, y);
      }
  }

  /// The last write to anything in `[base, end)` that the picture knows
  /// of, at its cells' grain.
  lastWrite(base, end) {
    let last = 0;
    for (const ram of this.rams ?? []) {
      const from = Math.max(base, ram.base);
      const to = Math.min(end, ram.base + ram.length);
      for (let c = Math.floor((from - ram.base) / CELL); c * CELL + ram.base < to; c++) last = Math.max(last, ram.stamps[c]);
    }
    return last;
  }

  /// Everything drawn again: what the page asks for when something it
  /// shows was changed by a click — a choice, the mark, the heat's pace.
  draw() {
    this.drawHead();
    this.drawCrumbs();
    this.drawMaps();
    this.drawOwners();
  }

  drawHead() {
    const halves = HALVES.map(([us, label]) => `<button type="button" data-half="${us}"${us === this.half ? ' class="on"' : ""}>${label}</button>`).join("");
    set(this.element.querySelector(".mem-halves"), `<span class="dim">heat halves every</span> ${halves}`);
    const mark = this.element.querySelector("[data-mark]");
    mark.classList.toggle("on", this.mark !== null);
    mark.textContent = this.mark === null ? "mark" : `marked ${age(Math.max(0, this.now - this.mark) )} ago · clear`;
    set(this.element.querySelector(".mem-note"), this.instant === null ? "live, in the board's time" : "as it was at the instant the board stands at");
  }

  /// The way down to the picture shown: each zoom a crumb to go back to.
  drawCrumbs() {
    const crumbs = [`<a data-crumb="0"${this.zooms.length ? "" : ' class="here"'}>all RAM</a>`];
    this.zooms.forEach((zoom, n) => {
      const here = n === this.zooms.length - 1;
      crumbs.push(`<a data-crumb="${n + 1}"${here ? ' class="here"' : ""} title="${this.escape(zoom.name)}">${this.escape(zoom.name)} <span class="dim">0x${zoom.base.toString(16)}–0x${(zoom.base + zoom.length).toString(16)}${here ? ` · ${zoom.cell} B a cell` : ""}</span></a>`);
    });
    // From a box to the whole of the memory it is part of.
    const zoom = this.zooms.at(-1);
    const block = zoom?.block
      ? ` <span class="controls"><button type="button" data-block title="the box shows only some of the memory from its first cell to its last: zoom out to the aligned block that run rounds out to, all of it">⤢ whole block 0x${zoom.block.base.toString(16)}–0x${(zoom.block.base + zoom.block.length).toString(16)} · ${size(zoom.block.length)}</button></span>`
      : "";
    const hint = this.zooms.length ? "drag a box to zoom further (Shift: its aligned block); a crumb goes back" : "drag a box to zoom in on it (Shift: the aligned block its memory rounds out to); a click shows what is there";
    set(this.element.querySelector(".mem-crumbs"), `${crumbs.join(`<span class="sep">›</span>`)}${block} <span class="dim hint">${hint}</span>`);
  }

  /// Back out to the `depth`th crumb, and then in on `into`, if given: a
  /// rectangle of a RAM's picture, as finely as it can be drawn.
  zoom(depth, into = null) {
    this.zooms.length = depth;
    if (into) this.zooms.push(into);
    this.detail = null;
    this.generation++;
    this.drawCrumbs();
    this.element.querySelector(".mem-maps").replaceChildren();
    this.polled = 0;
    if (this.busy) this.again = true;
    this.poll(true);
  }

  /// The box that finds a symbol of the board's code by name, and opens
  /// its source: the arrows choose among what it found, Enter opens it,
  /// Esc lets it go.
  equipSearch(input, list) {
    let found = [];
    let chosen = 0;
    const show = () => {
      list.hidden = !found.length && !input.value.trim();
      if (!input.value.trim()) return (list.innerHTML = "");
      const e = this.escape;
      list.innerHTML = found.length
        ? found.map((s, n) => `<div class="sym-row${n === chosen ? " on" : ""}" data-sym="${n}" title="${e(s.file)}:${s.line}"><b>${e(s.name)}</b> <span class="dim">${s.kind}${s.path !== s.name ? ` · ${e(s.path)}` : ""} · ${e(shortFile(s.file))}:${s.line}</span></div>`).join("")
        : `<div class="dim sym-row">${this.inspector.index ? "nothing by that name" : this.inspector.indexError ? `no index: ${e(this.inspector.indexError)}` : "reading the index…"}</div>`;
    };
    const open = (s) => {
      if (!s) return;
      list.hidden = true;
      this.inspector.openSource(s.file, s.line);
    };
    input.oninput = async () => {
      await this.inspector.loadIndex();
      found = search(this.inspector.index, input.value);
      chosen = 0;
      show();
    };
    input.onfocus = () => {
      this.inspector.loadIndex();
      if (input.value.trim()) show();
    };
    input.onkeydown = (event) => {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        chosen = Math.max(0, Math.min(found.length - 1, chosen + (event.key === "ArrowDown" ? 1 : -1)));
        show();
        list.querySelector(".on")?.scrollIntoView({ block: "nearest" });
        event.preventDefault();
      } else if (event.key === "Enter") open(found[chosen]);
      else if (event.key === "Escape") {
        list.hidden = true;
        event.stopPropagation();
      }
    };
    // Chosen with the pointer before the box, losing it, hides the list.
    list.onmousedown = (event) => {
      const row = event.target.closest("[data-sym]");
      if (row) open(found[Number(row.dataset.sym)]);
      event.preventDefault();
    };
    input.onblur = () => (list.hidden = true);
  }

  /// The pictures shown: the RAMs, or the part of one zoomed in on.
  shown() {
    if (this.zooms.length) return this.detail ? [this.detail] : [];
    return this.rams ?? [];
  }

  drawMaps() {
    this.drawField();
    const box = this.element.querySelector(".mem-maps");
    const pictures = this.shown();
    if (!pictures.length) return;
    const zoomed = !!this.zooms.length;
    if (box.childElementCount !== pictures.length || box.zoomed !== zoomed) {
      box.zoomed = zoomed;
      box.innerHTML = pictures
        .map((p, n) => `<figure class="mem-ram"><figcaption>${zoomed ? "" : `<b>${this.escape(p.name)}</b> <span class="dim">${size(p.length)} · 0x${p.base.toString(16).padStart(8, "0")}</span>`}</figcaption><div class="mem-frame"><canvas data-picture="${n}"></canvas><div class="mem-run"></div><div class="mem-drag"></div></div></figure>`)
        .join("");
      for (const canvas of box.querySelectorAll("canvas")) this.equip(canvas);
    }
    // A zoomed rectangle as large as the panel lets it be, its shape kept.
    const fit = (p) => {
      if (!zoomed) return SCALE;
      const wide = Math.min(1200, (box.clientWidth || 900) - 4);
      return Math.max(1, Math.min(wide / p.rect.w, 600 / p.rect.h));
    };
    pictures.forEach((p, n) => this.paint(box.querySelector(`canvas[data-picture="${n}"]`), p, fit(p)));
  }

  paint(canvas, picture, scale) {
    const chosen = this.owners.findIndex((o) => o.name === this.selected);
    const colors = this.owners.map((o) => rgbOf(o.color));
    const { view, rect } = picture;
    // Whole pixels a cell, at least one; the canvas stretched to the rest.
    const s = Math.max(1, Math.floor(scale));
    const w = rect.w * s;
    const h = rect.h * s;
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
    }
    canvas.style.width = `${Math.round(rect.w * scale)}px`;
    const g = canvas.getContext("2d");
    const image = g.createImageData(w, h);
    const px = image.data;
    const put = (x, y, rgb) => {
      const i = 4 * (y * w + x);
      px[i] = rgb[0];
      px[i + 1] = rgb[1];
      px[i + 2] = rgb[2];
      px[i + 3] = 255;
    };
    const statics = picture.statics;
    const at = (x, y) => (x < 0 || y < 0 || x >= rect.w || y >= rect.h ? -1 : view.at[(rect.y + y) * view.width + rect.x + x]);
    // Where one owner's memory meets another's, a line; where one static
    // meets another, a fainter one.
    const edge = (a, b) => {
      if (b < 0) return null;
      if (picture.owner[a] !== picture.owner[b]) return [0, 0, 0];
      if (statics && statics[a] !== statics[b]) return [90, 96, 108];
      return null;
    };
    const lines = s >= 3;
    const field = this.fieldOf(picture);
    this.eachCell(picture, (c, x, y) => {
      const owner = picture.owner[c];
      const stamp = picture.stamps[c];
      let rgb = BACKGROUND;
      if (owner >= 0) rgb = mix(BACKGROUND, colors[owner], picture.stack[c] ? 0.22 : statics && statics[c] < 0 ? 0.3 : 0.4);
      else if (stamp) rgb = [44, 48, 56];
      if (stamp && owner >= 0) rgb = mix(rgb, colors[owner], 0.15);
      if (stamp) {
        const hh = heat(this.now - stamp, this.half);
        if (hh > 0.02) rgb = mix(rgb, hot(hh), Math.min(1, 0.25 + hh));
      }
      if (chosen >= 0 && owner !== chosen) rgb = mix(BACKGROUND, rgb, 0.35);
      // Heat laid on: what it is of, hot; the rest dimmed, without its
      // writes' heat, which would read as the same.
      if (field) {
        const f = fieldHeat(field.weights[c], field.most);
        rgb = f ? hot(f) : mix(BACKGROUND, owner >= 0 ? colors[owner] : [44, 48, 56], 0.1);
      }
      const marked = this.mark !== null && stamp > this.mark;
      const right = edge(c, at(x + 1, y));
      const below = edge(c, at(x, y + 1));
      for (let dy = 0; dy < s; dy++)
        for (let dx = 0; dx < s; dx++) {
          let colour = rgb;
          if (marked && (dx === 0 || dy === 0)) colour = [80, 220, 255];
          else if (right && dx === s - 1 && (lines || right[0] === 0)) colour = right;
          else if (below && dy === s - 1 && (lines || below[0] === 0)) colour = below;
          put(x * s + dx, y * s + dy, colour);
        }
    });
    g.putImageData(image, 0, 0);
    // Names where most of what they name is: each owner's, and zoomed in
    // far enough, each static's — in the canvas's pixels, which the page
    // may stretch by `scale / s`.
    const label = (of, names, least, font) => {
      const sums = new Map();
      this.eachCell(picture, (c, x, y) => {
        const n = of[c];
        if (n < 0) return;
        const sum = sums.get(n) ?? { x: 0, y: 0, n: 0 };
        sum.x += x;
        sum.y += y;
        sum.n++;
        sums.set(n, sum);
      });
      g.font = font;
      g.textAlign = "center";
      g.textBaseline = "middle";
      g.lineWidth = 3;
      g.strokeStyle = "#000c";
      g.fillStyle = "#fff";
      for (const [n, sum] of sums) {
        const name = names[n];
        const area = sum.n * s * s;
        if (area < least || g.measureText(name).width > Math.sqrt(area) * 2.2) continue;
        const x = (sum.x / sum.n + 0.5) * s;
        const y = (sum.y / sum.n + 0.5) * s;
        g.strokeText(name, x, y);
        g.fillText(name, x, y);
      }
    };
    if (statics) label(statics, picture.names, 1200, "10px ui-monospace, monospace");
    label(picture.owner, this.owners.map((o) => o.name), 900, statics ? "bold 12px ui-monospace, monospace" : "10px ui-monospace, monospace");
    this.overlay(g, picture, s);
  }

  /// The heat laid on `picture`, its cells', worked out once a field.
  fieldOf(picture) {
    if (!this.field || !this.fieldShown) return null;
    if (picture.field?.of !== this.field) picture.field = { of: this.field, ...fieldCells(picture.base, picture.length, picture.cell, this.field.points) };
    return picture.field;
  }

  /// Heat to lay on the pictures: `{ label, back, points }` — what it is
  /// of, in words; whether it is what a value depended on rather than
  /// where it went; and `points` as `fieldCells` takes them — or null.
  /// Redrawn when it changes.
  setField(field) {
    const key = field ? `${field.label} ${field.back} ${field.points.length}` : "";
    if (key === this.fieldKey) return;
    if (field?.label !== this.field?.label) this.fieldShown = true;
    this.fieldKey = key;
    this.field = field ?? null;
    this.drawMaps();
  }

  /// The field's legend: what it is, its scale, and what of it is in no
  /// picture — flash, a NOR, a peripheral.
  drawField() {
    const box = this.element.querySelector(".mem-field");
    const field = this.field;
    if (!field) return set(box, "");
    const e = this.escape;
    const pictured = (p) => (this.rams ?? []).some((r) => p.addr < r.base + r.length && p.addr + p.length > r.base);
    const { outside } = fieldCells(0, 0, 1, field.points.filter((p) => !pictured(p)));
    const most = [...outside].sort((a, b) => b[1] - a[1]);
    const shown = most.slice(0, 4).map(([name, w]) => `${e(name)} ×${Math.round(w)}`);
    const rest = most.length > 4 ? ` and ${most.length - 4} more` : "";
    const scale = `<span class="field-scale" title="each cell as hot as how many ${field.back ? "values it depended on were there" : "of its bytes were written with something made of it"}, on a scale of logarithms"></span>`;
    const away = shown.length ? ` <span class="dim">· not in the pictures: ${shown.join(", ")}${rest}</span>` : "";
    set(box, `<span class="dim">heat:</span> <b>${e(field.label)}</b> ${scale}${away} <span class="controls"><button type="button" data-field-shown${this.fieldShown ? ' class="on"' : ""} title="show or hide it on the pictures">${this.fieldShown ? "shown" : "hidden"}</button></span>`);
  }

  /// The static the inspector's address is in, as `[base, size]`: the
  /// smallest of those known that holds it; or just the address.
  hereRun() {
    const at = this.focus;
    let best = null;
    for (const statics of this.symbols?.values() ?? [])
      for (const st of statics) if (at >= st.base && at < st.base + st.size && (!best || st.size < best.size)) best = st;
    return best ? [best.base, best.size] : [at, 1];
  }

  /// Where the inspector is, on a picture: the static its address is in
  /// outlined, the address ringed with guides across the picture to it,
  /// and the places it was before ringed fainter, numbered back from it.
  drawHere(g, picture, s, placeOf) {
    if (this.focus === null) return;
    const { view, rect } = picture;
    const centre = (p) => [p[0] + s / 2, p[1] + s / 2];
    // The places it was: 1 the latest.
    this.visits
      .slice()
      .reverse()
      .forEach((address, n) => {
        const p = placeOf(address);
        if (!p) return;
        const [x, y] = centre(p);
        const fade = 0.85 - n * 0.12;
        g.lineWidth = 2;
        g.strokeStyle = `rgb(200 220 255 / ${fade})`;
        g.beginPath();
        g.arc(x, y, Math.max(6, s * 1.2), 0, 2 * Math.PI);
        g.stroke();
        g.font = "bold 10px ui-monospace, monospace";
        g.textAlign = "center";
        g.textBaseline = "middle";
        g.lineWidth = 3;
        g.strokeStyle = "#000c";
        const label = String(n + 1);
        const [lx, ly] = [x + Math.max(6, s * 1.2) + 4, y - Math.max(6, s * 1.2)];
        g.strokeText(label, lx, ly);
        g.fillStyle = `rgb(200 220 255 / ${fade})`;
        g.fillText(label, lx, ly);
      });
    // The static it is in: its edge, where a cell of it meets one not.
    const [base, size] = this.hereRun();
    const first = Math.max(0, Math.floor((base - picture.base) / picture.cell));
    const last = Math.min(view.cells - 1, Math.floor((base + size - 1 - picture.base) / picture.cell));
    if (last >= first && last - first < 1 << 16) {
      const inside = (gx, gy) => {
        const c = gx < 0 || gy < 0 || gx >= view.width ? -1 : view.at[gy * view.width + gx];
        return c !== undefined && c >= first && c <= last;
      };
      g.strokeStyle = HERE;
      g.lineWidth = Math.max(1.5, s / 4);
      g.beginPath();
      for (let c = first; c <= last; c++) {
        const gx = view.place[2 * c];
        const gy = view.place[2 * c + 1];
        const x = (gx - rect.x) * s;
        const y = (gy - rect.y) * s;
        if (x < -s || y < -s || x > rect.w * s || y > rect.h * s) continue;
        if (!inside(gx, gy - 1)) (g.moveTo(x, y), g.lineTo(x + s, y));
        if (!inside(gx, gy + 1)) (g.moveTo(x, y + s), g.lineTo(x + s, y + s));
        if (!inside(gx - 1, gy)) (g.moveTo(x, y), g.lineTo(x, y + s));
        if (!inside(gx + 1, gy)) (g.moveTo(x + s, y), g.lineTo(x + s, y + s));
      }
      g.stroke();
    }
    // The address: guides across the picture, and a ring.
    const p = placeOf(this.focus);
    if (!p) return;
    const [x, y] = centre(p);
    g.save();
    g.setLineDash([4, 4]);
    g.lineWidth = 1;
    g.strokeStyle = "rgb(76 194 255 / 0.55)";
    g.beginPath();
    g.moveTo(0, y);
    g.lineTo(rect.w * s, y);
    g.moveTo(x, 0);
    g.lineTo(x, rect.h * s);
    g.stroke();
    g.restore();
    const r = Math.max(9, s * 2);
    g.lineWidth = 4;
    g.strokeStyle = "#000a";
    g.beginPath();
    g.arc(x, y, r, 0, 2 * Math.PI);
    g.stroke();
    g.lineWidth = 2.5;
    g.strokeStyle = HERE;
    g.beginPath();
    g.arc(x, y, r, 0, 2 * Math.PI);
    g.stroke();
  }

  /// What the inspector asked to be shown on the pictures: the memory a
  /// source line wrote, outlined, and where the chosen task's pointers
  /// point, as arrows from each pointer to its target.
  overlay(g, picture, s) {
    const { view, rect } = picture;
    // A cell's place on the canvas, in its pixels: its centre, or null when
    // it is outside the rectangle shown.
    const placeOf = (address) => {
      if (address < picture.base || address >= picture.base + picture.length) return null;
      const c = Math.floor((address - picture.base) / picture.cell);
      const x = view.place[2 * c] - rect.x;
      const y = view.place[2 * c + 1] - rect.y;
      return x < 0 || y < 0 || x >= rect.w || y >= rect.h ? null : [x * s, y * s];
    };
    // Few cells hot, each ringed, that one cell of a big picture is seen.
    const field = this.fieldOf(picture);
    if (field) {
      const rings = [];
      this.eachCell(picture, (c, x, y) => field.weights[c] > 0 && rings.length <= 64 && rings.push([x, y, fieldHeat(field.weights[c], field.most)]));
      if (rings.length <= 64) {
        g.lineWidth = 2;
        for (const [x, y, f] of rings) {
          g.strokeStyle = `rgb(${hot(f).join(" ")})`;
          g.beginPath();
          g.arc((x + 0.5) * s, (y + 0.5) * s, Math.max(7, s * 1.5), 0, 2 * Math.PI);
          g.stroke();
        }
      }
    }
    this.drawHere(g, picture, s, placeOf);
    if (this.highlight) {
      g.lineWidth = Math.max(1, s / 3);
      g.strokeStyle = "#fff";
      for (const [address, length] of this.highlight)
        for (let a = address - ((address - picture.base) % picture.cell); a < address + length; a += picture.cell) {
          const at = placeOf(a);
          if (at) g.strokeRect(at[0] + 0.5, at[1] + 0.5, s - 1, s - 1);
        }
    }
    g.strokeStyle = "#7ee787";
    g.fillStyle = "#7ee787";
    g.lineWidth = Math.max(1, s / 4);
    for (const [from, to] of this.arrows) {
      const a = placeOf(from);
      const b = placeOf(to);
      if (!a || !b || (a[0] === b[0] && a[1] === b[1])) continue;
      const [x0, y0, x1, y1] = [a[0] + s / 2, a[1] + s / 2, b[0] + s / 2, b[1] + s / 2];
      g.beginPath();
      g.moveTo(x0, y0);
      g.lineTo(x1, y1);
      g.stroke();
      const angle = Math.atan2(y1 - y0, x1 - x0);
      const head = Math.max(4, s);
      g.beginPath();
      g.moveTo(x1, y1);
      g.lineTo(x1 - head * Math.cos(angle - 0.4), y1 - head * Math.sin(angle - 0.4));
      g.lineTo(x1 - head * Math.cos(angle + 0.4), y1 - head * Math.sin(angle + 0.4));
      g.fill();
    }
  }

  /// Where a point of the page is in a picture's grid — the whole grid's
  /// coordinates — and the cell there, if any; the nearest place in the
  /// rectangle with `clamp`.
  cellAt(canvas, x, y, clamp = false) {
    const picture = this.shown()[Number(canvas.dataset.picture)];
    if (!picture) return null;
    const { rect, view } = picture;
    const box = canvas.getBoundingClientRect();
    let gx = Math.floor(((x - box.left) / box.width) * rect.w);
    let gy = Math.floor(((y - box.top) / box.height) * rect.h);
    if (clamp) {
      gx = Math.max(0, Math.min(rect.w - 1, gx));
      gy = Math.max(0, Math.min(rect.h - 1, gy));
    }
    if (gx < 0 || gy < 0 || gx >= rect.w || gy >= rect.h) return null;
    gx += rect.x;
    gy += rect.y;
    const c = view.at[gy * view.width + gx];
    return { picture, c, gx, gy, address: c < 0 ? null : picture.base + c * picture.cell };
  }

  equip(canvas) {
    const readout = () => this.element.querySelector(".mem-readout");
    const drag = canvas.parentElement.querySelector(".mem-drag");
    const run = canvas.parentElement.querySelector(".mem-run");
    canvas.onmousemove = (event) => {
      if (this.dragging) return;
      const hit = this.cellAt(canvas, event.clientX, event.clientY);
      if (!hit || hit.c < 0) return (readout().textContent = "\u00a0");
      const { picture, c, address } = hit;
      const owner = this.owners[picture.owner[c]];
      const stamp = picture.stamps[c];
      const what = this.named(address);
      const written = stamp ? `written ${age(this.now - stamp)} ago` : "never written";
      const inside = picture.statics && picture.statics[c] >= 0 ? picture.fullNames[picture.statics[c]] : null;
      const field = this.fieldOf(picture);
      const weight = field?.weights[c] ? ` · ${this.field.back ? `${+field.weights[c].toFixed(1)} of what it depended on` : `${+field.weights[c].toFixed(1)} bytes written with what was made of it`}` : "";
      readout().textContent = `0x${address.toString(16).padStart(8, "0")}–0x${(address + picture.cell - 1).toString(16)} · ${what ?? inside ?? (owner ? owner.name : picture.name)} · ${written}${weight}`;
    };
    canvas.onmouseleave = () => this.dragging || (readout().textContent = "\u00a0");
    // A press and a release in place is a click: what is there, to the
    // inspector. Dragged, a box: just that rectangle, zoomed in on.
    canvas.onmousedown = (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      const start = { x: event.clientX, y: event.clientY };
      this.dragging = true;
      const frame = canvas.getBoundingClientRect();
      const box = (e) => {
        const x0 = Math.max(frame.left, Math.min(start.x, e.clientX));
        const x1 = Math.min(frame.right, Math.max(start.x, e.clientX));
        const y0 = Math.max(frame.top, Math.min(start.y, e.clientY));
        const y1 = Math.min(frame.bottom, Math.max(start.y, e.clientY));
        return { x0, x1, y0, y1 };
      };
      // What a release would zoom on: the box, or with Shift held, the
      // aligned block its memory rounds out to, the box snapped to it. The
      // block is outlined, dashed, either way: where Shift would take it.
      let last = null;
      const place = (element, at) => {
        if (!at) return (element.style.display = "none");
        Object.assign(element.style, { display: "block", left: `${at.left}px`, top: `${at.top}px`, width: `${at.width}px`, height: `${at.height}px` });
      };
      const show = (e) => {
        const b = box(e);
        const boxed = this.boxed(canvas, b);
        const block = boxed && this.blockZoom(boxed);
        const snapped = !!(e.shiftKey && block);
        last = { clientX: e.clientX, clientY: e.clientY };
        drag.classList.toggle("snapped", snapped);
        place(drag, snapped ? this.onCanvas(canvas, block.rect, block.cell) : { left: b.x0 - frame.left, top: b.y0 - frame.top, width: b.x1 - b.x0, height: b.y1 - b.y0 });
        place(run, block ? this.onCanvas(canvas, block.rect, block.cell) : null);
        if (!boxed) return (readout().textContent = "nothing there");
        const rounded = block ? `0x${block.base.toString(16)}–0x${(block.base + block.length).toString(16)}, ${size(block.length)}` : "";
        readout().textContent = snapped
          ? `the aligned block ${rounded} — let go to zoom on it; let go of Shift for just the box`
          : `${boxed.text} — let go to zoom on the box${block ? `; hold Shift for the aligned block it rounds out to, ${rounded} (dashed)` : ""}`;
      };
      const moved = (e) => show(e);
      const keyed = (e) => e.key === "Shift" && last && show({ ...last, shiftKey: e.type === "keydown" });
      const released = (e) => {
        window.removeEventListener("mousemove", moved);
        window.removeEventListener("mouseup", released);
        window.removeEventListener("keydown", keyed);
        window.removeEventListener("keyup", keyed);
        this.dragging = false;
        drag.style.display = "none";
        run.style.display = "none";
        drag.classList.remove("snapped");
        readout().textContent = "\u00a0";
        const far = Math.abs(e.clientX - start.x) > 4 || Math.abs(e.clientY - start.y) > 4;
        if (!far) return this.pick(canvas, e);
        const boxed = this.boxed(canvas, box(e));
        const block = e.shiftKey && boxed && this.blockZoom(boxed);
        if (block || boxed) this.zoom(this.zooms.length, block || boxed);
      };
      window.addEventListener("mousemove", moved);
      window.addEventListener("mouseup", released);
      window.addEventListener("keydown", keyed);
      window.addEventListener("keyup", keyed);
    };
  }

  /// A box on a picture as a zoom: the same rectangle of the RAM's
  /// picture.
  boxed(canvas, { x0, x1, y0, y1 }) {
    const a = this.cellAt(canvas, x0, y0, true);
    const b = this.cellAt(canvas, x1 - 1, y1 - 1, true);
    if (!a || !b) return null;
    const ram = this.rams.findIndex((r) => r.base === a.picture.base);
    const rect = { x: Math.min(a.gx, b.gx), y: Math.min(a.gy, b.gy), w: Math.abs(b.gx - a.gx) + 1, h: Math.abs(b.gy - a.gy) + 1 };
    return this.zoomFor(ram, rect, a.picture.cell);
  }

  /// A RAM's layout at `cell` bytes a cell.
  layoutOf(ram, cell) {
    const key = `${ram.length}/${cell}`;
    if (!this.layouts.has(key)) this.layouts.set(key, layout(ram.length, cell));
    return this.layouts.get(key);
  }

  /// A zoom on `rect` of the `ram`th RAM's picture at `cell` bytes a cell:
  /// drawn as finely as it can be with no more than [`ZOOM_CELLS`] — a
  /// cell of `n` bytes is a square of four of `n / 4`, in its place, down
  /// to a word a cell — with the run of memory its cells take in, to
  /// read, and the aligned block that run rounds out to.
  zoomFor(ram, rect, cell) {
    const picture = this.rams?.[ram];
    if (!picture || rect.w < 1 || rect.h < 1) return null;
    ({ rect, cell } = refine(rect, cell, ZOOM_CELLS));
    let view = this.layoutOf(picture, cell);
    let run = runOf(view, rect);
    // The run of memory a box takes in may be longer than the box: kept
    // within what one read answers at this grain, coarser if need be.
    while (run && run.last - run.first + 1 > ZOOM_CELLS && cell < CELL) {
      ({ rect, cell } = coarser(rect, cell));
      view = this.layoutOf(picture, cell);
      run = runOf(view, rect);
    }
    if (!run) return null;
    const { first, last, count } = run;
    const base = picture.base + first * cell;
    const length = (last - first + 1) * cell;
    const owners = [...new Set(this.owners.filter((o) => o.ranges.some((r) => r.base < base + length && r.end > base)).map((o) => o.name))];
    const name = `${picture.name}${owners.length && owners.length <= 3 ? ` · ${owners.join(", ")}` : ""}`;
    const block = rounded(base, length, picture);
    // The box is its block already when it takes in the whole of it.
    const whole = block.length === length && block.base === base && last - first + 1 === count;
    return { ram, cell, rect, first, base, length, name, block: whole ? null : block, text: `${rect.w}×${rect.h} cells of ${cell} B, within 0x${base.toString(16)}–0x${(base + length).toString(16)}` };
  }

  /// The zoom on the aligned block a zoom's run rounds out to: the
  /// rectangle around its cells.
  blockZoom(zoom) {
    const picture = this.rams?.[zoom?.ram];
    if (!picture || !zoom.block) return null;
    const from = (zoom.block.base - picture.base) / 4;
    const rect = rectAround(this.layoutOf(picture, 4), from, from + zoom.block.length / 4);
    return this.zoomFor(zoom.ram, rect, 4);
  }

  /// Where `rect`, of a RAM's picture at `cell` bytes a cell, is on
  /// `canvas`, in its pixels from its corner.
  onCanvas(canvas, rect, cell) {
    const picture = this.shown()[Number(canvas.dataset.picture)];
    if (!picture) return null;
    const box = canvas.getBoundingClientRect();
    return rectOnBox(rect, cell, picture.rect, picture.cell, box.width, box.height);
  }

  /// Moves what the inspector shows to `address`, keeping where it was.
  visit(address) {
    if (this.focus !== null && this.focus !== address) {
      this.visits = this.visits.filter((a) => a !== this.focus && a !== address);
      this.visits.push(this.focus);
      this.visits = this.visits.slice(-VISITS);
    }
    this.focus = address;
    // Whose it is: their statics, for the one it is in to be outlined.
    const owner = this.owners.find((o) => o.ranges.some((r) => address >= r.base && address < r.end));
    if (owner && !this.symbols.has(owner.name)) {
      this.symbols.set(owner.name, []);
      this.read(`board/symbols/${owner.name}`)
        .then((answer) => {
          this.symbols.set(owner.name, answer?.statics ?? []);
          this.drawMaps();
        })
        .catch(() => {});
    }
  }

  pick(canvas, event) {
    const hit = this.cellAt(canvas, event.clientX, event.clientY);
    if (!hit || hit.c < 0) return;
    const owner = this.owners[hit.picture.owner[hit.c]];
    this.selected = owner?.name ?? null;
    this.visit(hit.address);
    this.inspected = null;
    this.draw();
    this.inspect();
  }

  /// What `address` is, as `board/whatis` says, asked for once.
  named(address) {
    if (this.whatis.has(address)) return this.whatis.get(address);
    this.whatis.set(address, null);
    this.read(`board/whatis/${address}`)
      .then((answer) => answer?.text && this.whatis.set(address, answer.text))
      .catch(() => {});
    return null;
  }

  /// An owner's stack as `board/tasks` last said: with how deep it has been.
  stackOf(name) {
    return this.tasks && [this.tasks.kernel, ...this.tasks.tasks].find((t) => t.name === name)?.stack;
  }

  drawOwners() {
    const box = this.element.querySelector(".mem-owners");
    if (!this.owners.length) {
      const why = this.tasks === null ? "reading the board's archive…" : `nothing says what this memory is: ${this.escape(this.tasks.error ?? "the archive has no tasks")}`;
      return set(box, `<div class="dim">${why}</div>`);
    }
    const rows = this.owners
      .map((o) => {
        const bytes = o.ranges.reduce((n, r) => n + r.end - r.base, 0);
        // Its own ranges only: not whatever lies between them.
        const last = Math.max(0, ...o.ranges.map((r) => this.lastWrite(r.base, r.end)));
        const glow = last ? heat(this.now - last, this.half) : 0;
        let gauge = "";
        if (o.stack) {
          const deepest = this.stackOf(o.name)?.deepest;
          const used = deepest ? o.stack.base + o.stack.size - deepest : 0;
          const share = used / o.stack.size;
          gauge = `<span class="mem-gauge${share > 0.85 ? " tight" : ""}" title="its stack: ${used} of ${o.stack.size} bytes reached, at the deepest"><span style="width:${(100 * share).toFixed(1)}%;background:${o.color}"></span></span> <span class="dim">${used}/${o.stack.size} B</span>`;
        }
        const [r, g, b] = hot(glow);
        const doing = o.index !== null && o.index !== undefined ? this.inspector.schedules[o.index] : null;
        if (doing) gauge = `<span class="dim doing" title="${this.escape(this.inspector.scheduleText(doing))}">${this.escape(this.inspector.scheduleText(doing))}</span>${gauge}`;
        return `<div class="mem-owner${o.name === this.selected ? " on" : ""}" data-owner="${this.escape(o.name)}"><span class="swatch" style="background:${o.color}"></span><span class="glow" style="background:rgb(${r} ${g} ${b} / ${glow.toFixed(2)})" title="${last ? `last wrote ${age(this.now - last)} ago` : "never written"}"></span><b>${this.escape(o.name)}</b> <span class="dim">${(bytes / 1024).toFixed(1)} KiB</span> ${gauge}</div>`;
      })
      .join("");
    set(box, `<div class="dim mem-label">whose memory it is · how lately each wrote · what each is doing · how deep each stack has been</div><div class="mem-owner-grid">${rows}</div>`);
  }

  /// Reads what the inspector shows (the chosen owner's statics, typed
  /// where its DWARF says what they are, and its stack; or the bytes
  /// around an address nothing describes) when what it covers was written
  /// since it last read them: each cluster on its own, a word's start to a
  /// word's end, so what lies between them is neither read nor watched.
  async inspect() {
    const owner = this.owners.find((o) => o.name === this.selected);
    if (!owner && this.focus === null) {
      this.inspected = null;
      return this.inspector.draw(true);
    }
    // A later call — another choice, another instant — supersedes this one.
    const token = ++this.inspecting;
    const generation = this.generation;
    const stale = () => token !== this.inspecting || generation !== this.generation;
    const key = `${this.selected} ${this.focus}`;
    let statics = [];
    if (owner) {
      const typed = await this.inspector.typedOf(owner.name);
      if (stale()) return;
      if (!this.symbols.has(owner.name)) {
        const answer = await this.read(`board/symbols/${owner.name}`).catch(() => null);
        if (stale()) return;
        this.symbols.set(owner.name, answer?.statics ?? []);
      }
      statics = (typed.statics ?? []).map((s) => ({ name: s.name, base: s.address, size: s.size, type: s.type }));
      // What the symbol table names that the DWARF does not type: its bytes.
      for (const s of this.symbols.get(owner.name)) if (!statics.some((t) => t.base === s.base)) statics.push({ ...s });
      statics.sort((a, b) => a.base - b.base);
      // Its stack, as deep as it has been: below that is only the paint.
      if (owner.stack) {
        const top = owner.stack.base + owner.stack.size;
        const deepest = this.stackOf(owner.name)?.deepest ?? owner.stack.base;
        statics.push({ name: "its stack, as deep as it has been", base: deepest, size: top - deepest, stack: true });
      }
    }
    // Around an address no static of its covers: its words, to be guessed at.
    const covered = (a) => statics.some((s) => !s.stack && a >= s.base && a < s.base + s.size);
    const around = this.focus !== null && !covered(this.focus) ? [{ name: `around 0x${this.focus.toString(16)}`, base: (this.focus & ~(AROUND - 1)) >>> 0, size: AROUND, around: true }] : [];
    statics = statics.filter((s) => s.size > 0);
    const runs = clusters([...statics, ...around], 256, INSPECTED);
    const last = Math.max(0, ...runs.map((r) => this.lastWrite(r.base, r.end)));
    if (this.inspected?.key === key && this.inspected.last === last) {
      this.inspector.draw();
      return this.inspector.refresh();
    }
    const chunks = [];
    const writers = [];
    for (const run of runs) {
      const chunk = { base: run.base, end: run.end, hex: "", words: [] };
      for (let at = run.base; at < run.end; at += SERVED) {
        const length = Math.min(SERVED, run.end - at);
        const [memory, written] = await Promise.all([this.read(`board/memory/${at}/${length}`), this.read(`board/written/${at}/${length}`)]);
        if (stale()) return;
        if (!memory?.hex || !written?.words) return;
        chunk.hex += memory.hex;
        const offset = writers.length;
        writers.push(...written.writers);
        for (let i = 0; i < written.words.length; i += 2) chunk.words.push(written.words[i], written.words[i + 1] < 0 ? -1 : written.words[i + 1] + offset);
      }
      chunks.push(chunk);
    }
    const read = (st) => runs.some((r) => st.base >= r.base && st.base < r.end);
    this.inspected = {
      key,
      last,
      chunks,
      writers,
      statics: statics.filter((st) => !st.stack && read(st)),
      around: around.filter(read),
      cut: statics.some((st) => !read(st) || !runs.some((r) => st.base + st.size <= r.end)),
      id: token,
    };
    this.inspector.draw();
    await this.inspector.refresh();
  }

  /// The chunk an address of what the inspector read is in.
  chunkOf(address) {
    return this.inspected?.chunks.find((c) => address >= c.base && address < c.end) ?? null;
  }

  /// The byte at `address`, as the inspector read it; null if it did not.
  byteAt(address) {
    const chunk = this.chunkOf(address);
    if (!chunk) return null;
    const i = 2 * (address - chunk.base);
    const pair = chunk.hex.slice(i, i + 2);
    return pair.length === 2 ? parseInt(pair, 16) : null;
  }

  /// The word at `address`: its stamp and writer's number, from the chunk
  /// it is in; `[0, -1]` for one not read.
  wordAt(address) {
    const chunk = this.chunkOf(address);
    if (!chunk) return [0, -1];
    const w = ((address - chunk.base) >> 2) * 2;
    return [chunk.words[w] ?? 0, chunk.words[w + 1] ?? -1];
  }

  /// The latest write to any word of `[from, to)` the inspector read: its
  /// stamp and writer's number.
  latestIn(from, to) {
    let best = [0, -1];
    for (let a = from - (from % 4); a < to; a += 4) {
      const word = this.wordAt(a);
      if (word[0] > best[0]) best = word;
    }
    return best;
  }

  stampOf(from, to) {
    return this.latestIn(from, to)[0];
  }

  heatOf(stamp) {
    return stamp ? heat(this.now - stamp, this.half) : 0;
  }

  hotColour(h, alpha) {
    const [r, g, b] = hot(h);
    return `rgb(${r} ${g} ${b} / ${alpha.toFixed(2)})`;
  }

  ago(stamp) {
    return age(Math.max(0, this.now - stamp));
  }

  /// A static nothing types, as its bytes.
  rawStaticHtml(st) {
    const e = this.escape;
    let bytes = "";
    const shown = Math.min(st.size, 64);
    for (let a = st.base; a < st.base + shown; a++) {
      if (a > st.base && (a - st.base) % 16 === 0) bytes += `<br><span class="addr">${(a - st.base).toString(16).padStart(4, "0")}</span> `;
      const b = this.byteAt(a);
      bytes += `<span class="b" data-heat="${a},${a + 1}" data-trail="${a}">${b === null ? "  " : b.toString(16).padStart(2, "0")}</span>`;
    }
    const more = st.size > shown ? `<span class="dim"> …${st.size - shown} more</span>` : "";
    return `<div class="mem-static"><div class="mem-static-head"><span class="name" title="${e(st.name)}">${e(shortName(st.name))}</span> <span class="dim">0x${st.base.toString(16)} · ${st.size} B · untyped</span>${st.size >= 16 ? ` <a data-view-raw="${st.base},${st.size}" data-name="${e(shortName(st.name))}" title="its bytes as a picture, hex or text: any of them a place to ask why">view</a>` : ""}</div><div class="mem-bytes">${bytes}${more}</div></div>`;
  }

  /// Shows `address`: its owner chosen, the inspector on it, and it
  /// outlined in the pictures.
  goTo(address) {
    const owner = this.owners.find((o) => o.ranges.some((r) => address >= r.base && address < r.end));
    this.selected = owner?.name ?? null;
    this.visit(address);
    this.inspected = null;
    this.highlight = null;
    this.draw();
    this.inspect();
  }

  /// Memory to outline in the pictures: runs of `[address, length]`.
  setHighlight(runs) {
    this.highlight = runs?.length ? runs : null;
    this.drawMaps();
  }

  /// Pointers to draw in the pictures, `[from, to]`: redrawn when they change.
  setArrows(arrows) {
    const key = arrows.map(([a, b]) => `${a}>${b}`).join(",");
    if (key === this.arrowsKey) return;
    this.arrowsKey = key;
    this.arrows = arrows;
    this.drawMaps();
  }

  clicked(event) {
    const t = (selector) => event.target.closest(selector);
    if (t(".mem-inspector") && this.inspector.clicked(event)) return;
    const crumb = t("[data-crumb]");
    if (crumb) return this.zoom(Number(crumb.dataset.crumb));
    if (t("[data-block]")) {
      const block = this.blockZoom(this.zooms.at(-1));
      if (block) this.zoom(this.zooms.length, block);
      return;
    }
    if (t("[data-half]")) {
      this.half = Number(t("[data-half]").dataset.half);
      this.draw();
      return this.inspector.draw();
    }
    if (t("[data-stand]")) {
      // The write was within this microsecond of the board's; its end is
      // just after it.
      return this.stand(Number(t("[data-stand]").dataset.stand) * 1e6);
    }
    if (t("[data-field-shown]")) {
      this.fieldShown = !this.fieldShown;
      return this.drawMaps();
    }
    if (t("[data-mark]")) {
      this.mark = this.mark === null ? this.now : null;
      // What every task's statics held at the mark, for what changed since.
      if (this.mark !== null) this.inspector.takeSnapshot();
      else this.inspector.snapshot = null;
      this.draw();
      return this.inspector.draw(true);
    }
    const owner = t("[data-owner]");
    if (owner) {
      this.selected = this.selected === owner.dataset.owner ? null : owner.dataset.owner;
      this.focus = null;
      this.inspected = null;
      this.highlight = null;
      this.draw();
      return this.inspect();
    }
  }
}

function mix(a, b, f) {
  return [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, a[2] + (b[2] - a[2]) * f];
}

/// How an element is brought to show new HTML: the page's own, which
/// keeps what is under the pointer in place, once it is given.
let shown = (element, html) => {
  element.innerHTML = html;
  element.shown = html;
};

/// `element` showing `html`, changed only when that changes.
function set(element, html) {
  if (!element || element.shown === html) return;
  shown(element, html);
}
