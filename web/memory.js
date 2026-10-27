// The board's memory, drawn: each RAM a picture whose cells are laid on a
// Hilbert curve, so a power-of-two region — what Hubris gives each task,
// for the MPU — is a square or a 2:1 block of its own; each cell coloured
// by whose it is, and lit by how lately it was written, fading in the
// board's own time. Under it, each task's stack and how deep it has been,
// and an inspector: the statics of whatever is chosen, each byte lit the
// same way, with the code that last wrote it, and the time of that write a
// handle on the instant.
//
// What it reads, from the board's store: `board/writes/{since}/{cell}` for
// the cells written since the last read, `board/tasks` and
// `board/symbols/{owner}` for what the memory is, `board/memory` and
// `board/written` for the bytes of what the inspector shows, and
// `board/whatis` for what the pointer is on.

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

export class MemoryView {
  /// `read(path)` reads the board's store at the instant in view;
  /// `stand(ps)` stands every panel at an instant of the board's.
  constructor(element, { read, stand, escape, colorOf, update }) {
    if (update) shown = update;
    this.element = element;
    this.read = read;
    this.stand = stand;
    this.escape = escape;
    this.colorOf = colorOf;
    this.half = 1e5;
    this.mark = null;
    this.selected = null; // an owner's name
    this.focus = null; // an address the inspector shows
    this.sort = "address";
    this.open = new Set(); // the statics shown whole
    this.zooms = []; // the way down: each a rectangle of a RAM's picture, and what it reads
    this.layouts = new Map(); // pictures' layouts, by RAM length and cell
    this.reset();
    this.image = null;
    this.owners = [];
    element.innerHTML = `<div class="mem-head"><span class="controls mem-halves"></span> <span class="controls"><button type="button" data-mark title="mark this instant: the cells written since are outlined, and the inspector can show only what was">mark</button></span> <span class="dim mem-note"></span></div>
      <div class="mem-crumbs"></div>
      <div class="mem-maps"></div>
      <div class="mem-readout dim">&nbsp;</div>
      <div class="mem-owners"></div>
      <div class="mem-inspector"></div>`;
    element.onclick = (event) => this.clicked(event);
    this.whatis = new Map();
  }

  reset() {
    this.rams = null;
    this.detail = null;
    this.now = 0;
    this.instant = undefined;
    this.inspected = null;
  }

  /// The board as the board panel last read it, and the instant the page
  /// stands at (`null`: live).
  update(board, standing) {
    if (board.image !== this.image) {
      this.image = board.image;
      this.reset();
      this.zooms = [];
      this.owners = [];
      this.symbols = new Map();
      this.whatis.clear();
      this.tasks = null;
      this.read("board/tasks")
        .then((tasks) => {
          if (tasks?.error) return;
          this.tasks = tasks;
          this.owners = owners(tasks, this.colorOf);
          for (const picture of [...(this.rams ?? []), this.detail].filter(Boolean)) Object.assign(picture, cellOwners(picture.base, picture.view, this.owners));
          this.draw();
        })
        .catch(() => {});
    }
    const instant = standing === null ? null : board.time_ps;
    // Stacks deepen as the board runs: their depths read again now and
    // then, and at each instant stood at.
    if (this.tasks && (instant !== this.instant || performance.now() - (this.tasksRead ?? 0) > TASKS_MS)) {
      this.tasksRead = performance.now();
      this.read("board/tasks")
        .then((tasks) => {
          if (tasks?.kernel && board.image === this.image) {
            this.tasks = tasks;
            this.drawOwners();
          }
        })
        .catch(() => {});
    }
    if (instant !== this.instant) {
      this.instant = instant;
      this.rams = null;
      this.detail = null;
      this.inspected = null;
      this.poll(true);
    } else this.poll(false);
  }

  /// A picture of a RAM — `board/writes`'s — at `cell` bytes a cell,
  /// showing `rect` of its grid, all of it if none.
  picture(ram, cell, rect = null) {
    const view = this.layoutOf(ram, cell);
    rect ??= { x: 0, y: 0, w: view.width, h: view.height };
    return { ...ram, cell, view, rect, stamps: new Uint32Array(view.cells), ...cellOwners(ram.base, view, this.owners), statics: null };
  }

  async poll(force) {
    const now = performance.now();
    if (this.busy || (!force && now - (this.polled ?? 0) < POLL_MS)) return;
    this.busy = true;
    this.polled = now;
    try {
      const since = this.rams ? this.now : 0;
      const zoom = this.zooms.at(-1);
      const [answer, detail] = await Promise.all([
        this.read(`board/writes/${since}/${CELL}`),
        zoom ? this.read(`board/writes/${this.detail ? this.now : 0}/${zoom.cell}/${zoom.base}/${zoom.length}`) : null,
      ]);
      if (!answer?.rams) return;
      // The board went back — a reset, a seek: everything again.
      if (this.rams && answer.now < this.now) {
        this.rams = null;
        this.detail = null;
        this.busy = false;
        return this.poll(true);
      }
      if (!this.rams) this.rams = answer.rams.map((ram) => this.picture(ram, answer.cell));
      const cells = answer.cells;
      for (let i = 0; i < cells.length; i += 3) this.rams[cells[i]].stamps[cells[i + 1]] = cells[i + 2];
      // The zoom may have changed while this was read: only the one asked
      // for is kept. Its cells are numbered from the start of what was
      // read, which is cell `first` of the RAM's.
      if (detail?.cells && zoom === this.zooms.at(-1)) {
        if (!this.detail) {
          this.detail = this.picture(this.rams[zoom.ram], zoom.cell, zoom.rect);
          this.nameStatics(this.detail);
        }
        for (let i = 0; i < detail.cells.length; i += 3) this.detail.stamps[zoom.first + detail.cells[i + 1]] = detail.cells[i + 2];
      }
      this.now = answer.now;
      this.draw();
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
    this.drawCrumbs();
    this.element.querySelector(".mem-maps").replaceChildren();
    this.polled = 0;
    if (this.busy) this.again = true;
    this.poll(true);
  }

  /// The pictures shown: the RAMs, or the part of one zoomed in on.
  shown() {
    if (this.zooms.length) return this.detail ? [this.detail] : [];
    return this.rams ?? [];
  }

  drawMaps() {
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
      readout().textContent = `0x${address.toString(16).padStart(8, "0")}–0x${(address + picture.cell - 1).toString(16)} · ${what ?? inside ?? (owner ? owner.name : picture.name)} · ${written}`;
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
    while (cell > 4 && rect.w * rect.h * 4 <= ZOOM_CELLS) {
      cell /= 4;
      rect = { x: rect.x * 2, y: rect.y * 2, w: rect.w * 2, h: rect.h * 2 };
    }
    while (rect.w * rect.h > ZOOM_CELLS && cell < CELL) {
      cell *= 4;
      const x = Math.floor(rect.x / 2);
      const y = Math.floor(rect.y / 2);
      rect = { x, y, w: Math.ceil((rect.x + rect.w) / 2) - x, h: Math.ceil((rect.y + rect.h) / 2) - y };
    }
    const view = this.layoutOf(picture, cell);
    let first = Infinity;
    let last = -1;
    for (let y = rect.y; y < rect.y + rect.h; y++)
      for (let x = rect.x; x < rect.x + rect.w; x++) {
        const c = view.at[y * view.width + x];
        if (c < 0) continue;
        first = Math.min(first, c);
        last = Math.max(last, c);
      }
    if (last < 0) return null;
    const base = picture.base + first * cell;
    const length = (last - first + 1) * cell;
    const owners = [...new Set(this.owners.filter((o) => o.ranges.some((r) => r.base < base + length && r.end > base)).map((o) => o.name))];
    const name = `${picture.name}${owners.length && owners.length <= 3 ? ` · ${owners.join(", ")}` : ""}`;
    const block = rounded(base, length, picture);
    // The box is its block already when it takes in the whole of it.
    const whole = block.length === length && block.base === base && (last - first + 1) === countIn(view, rect);
    return { ram, cell, rect, first, base, length, name, block: whole ? null : block, text: `${rect.w}×${rect.h} cells of ${cell} B, within 0x${base.toString(16)}–0x${(base + length).toString(16)}` };
  }

  /// The zoom on the aligned block a zoom's run rounds out to: the
  /// rectangle around its cells.
  blockZoom(zoom) {
    const picture = this.rams?.[zoom.ram];
    if (!picture || !zoom.block) return null;
    const view = this.layoutOf(picture, 4);
    const from = (zoom.block.base - picture.base) / 4;
    const to = from + zoom.block.length / 4;
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
    return this.zoomFor(zoom.ram, { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 }, 4);
  }

  /// Where `rect`, of a RAM's picture at `cell` bytes a cell, is on
  /// `canvas`, in its pixels from its corner: a rectangle drawn at one
  /// grain stands where it would at another, a cell's side halving as its
  /// bytes quarter.
  onCanvas(canvas, rect, cell) {
    const picture = this.shown()[Number(canvas.dataset.picture)];
    if (!picture) return null;
    const f = Math.sqrt(cell / picture.cell);
    const box = canvas.getBoundingClientRect();
    const px = box.width / picture.rect.w;
    const py = box.height / picture.rect.h;
    const left = Math.max(0, (rect.x * f - picture.rect.x) * px);
    const top = Math.max(0, (rect.y * f - picture.rect.y) * py);
    const right = Math.min(box.width, ((rect.x + rect.w) * f - picture.rect.x) * px);
    const bottom = Math.min(box.height, ((rect.y + rect.h) * f - picture.rect.y) * py);
    if (right <= left || bottom <= top) return null;
    return { left, top, width: right - left, height: bottom - top };
  }

  pick(canvas, event) {
    const hit = this.cellAt(canvas, event.clientX, event.clientY);
    if (!hit || hit.c < 0) return;
    const owner = this.owners[hit.picture.owner[hit.c]];
    this.selected = owner?.name ?? null;
    this.focus = hit.address;
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
    if (!this.owners.length) return set(box, `<div class="dim">${this.tasks === null ? "reading the board's archive…" : "no archive says what this memory is: the board's configuration names none"}</div>`);
    const rows = this.owners
      .map((o) => {
        const bytes = o.ranges.reduce((n, r) => n + r.end - r.base, 0);
        const last = this.lastWrite(o.ranges[0]?.base ?? 0, o.ranges.at(-1)?.end ?? 0);
        const glow = last ? heat(this.now - last, this.half) : 0;
        let gauge = "";
        if (o.stack) {
          const deepest = this.stackOf(o.name)?.deepest;
          const used = deepest ? o.stack.base + o.stack.size - deepest : 0;
          const share = used / o.stack.size;
          gauge = `<span class="mem-gauge${share > 0.85 ? " tight" : ""}" title="its stack: ${used} of ${o.stack.size} bytes reached, at the deepest"><span style="width:${(100 * share).toFixed(1)}%;background:${o.color}"></span></span> <span class="dim">${used}/${o.stack.size} B</span>`;
        }
        const [r, g, b] = hot(glow);
        return `<div class="mem-owner${o.name === this.selected ? " on" : ""}" data-owner="${this.escape(o.name)}"><span class="swatch" style="background:${o.color}"></span><span class="glow" style="background:rgb(${r} ${g} ${b} / ${glow.toFixed(2)})" title="${last ? `last wrote ${age(this.now - last)} ago` : "never written"}"></span><b>${this.escape(o.name)}</b> <span class="dim">${(bytes / 1024).toFixed(1)} KiB</span> ${gauge}</div>`;
      })
      .join("");
    set(box, `<div class="dim mem-label">whose memory it is · how lately each wrote · how deep each stack has been</div><div class="mem-owner-grid">${rows}</div>`);
  }

  /// Reads what the inspector shows — the chosen owner's statics, or the
  /// bytes around an address — when what it covers was written since it
  /// last read them.
  async inspect() {
    const box = this.element.querySelector(".mem-inspector");
    const owner = this.owners.find((o) => o.name === this.selected);
    if (!owner && this.focus === null) return set(box, `<div class="dim">click a task, or a place in the pictures, to see what is there</div>`);
    const key = `${this.selected} ${owner ? "" : this.focus}`;
    let statics = [];
    if (owner) {
      if (!this.symbols.has(owner.name)) {
        const answer = await this.read(`board/symbols/${owner.name}`).catch(() => null);
        this.symbols.set(owner.name, answer?.statics ?? []);
      }
      statics = [...this.symbols.get(owner.name)];
      // Its stack, as deep as it has been: below that is only the paint.
      if (owner.stack) {
        const top = owner.stack.base + owner.stack.size;
        const deepest = this.stackOf(owner.name)?.deepest ?? owner.stack.base;
        statics.push({ name: "its stack, as deep as it has been", base: deepest, size: top - deepest, stack: true });
      }
    } else statics = [{ name: `around 0x${this.focus.toString(16)}`, base: (this.focus & ~0xff) >>> 0, size: 256 }];
    statics = statics.filter((s) => s.size > 0);
    const base = Math.min(...statics.map((s) => s.base));
    const end = Math.min(base + INSPECTED, Math.max(...statics.map((s) => s.base + s.size)));
    const last = this.lastWrite(base, end);
    if (this.inspected?.key === key && this.inspected.last === last && this.inspected.now === this.now) return this.drawInspector();
    if (this.inspected?.key === key && this.inspected.last === last) {
      this.inspected.now = this.now;
      return this.drawInspector();
    }
    const hex = [];
    const words = [];
    const writers = [];
    for (let at = base; at < end; at += SERVED) {
      const length = Math.min(SERVED, end - at);
      const [memory, written] = await Promise.all([this.read(`board/memory/${at}/${length}`), this.read(`board/written/${at}/${length}`)]);
      if (!memory?.hex || !written?.words) return;
      hex.push(memory.hex);
      const offset = writers.length;
      writers.push(...written.writers);
      for (let i = 0; i < written.words.length; i += 2) words.push(written.words[i], written.words[i + 1] < 0 ? -1 : written.words[i + 1] + offset);
    }
    this.inspected = { key, base, end, last, now: this.now, hex: hex.join(""), words, writers, statics };
    this.drawInspector();
  }

  drawInspector() {
    const box = this.element.querySelector(".mem-inspector");
    const s = this.inspected;
    if (!s) return;
    const e = this.escape;
    const wordOf = (address) => ((address - (s.base & ~3)) >> 2) * 2;
    const byteHtml = (address) => {
      const i = 2 * (address - s.base);
      const text = s.hex.slice(i, i + 2) || "  ";
      const w = wordOf(address);
      const stamp = s.words[w];
      if (!stamp) return `<span class="b cold">${text}</span>`;
      const h = heat(this.now - stamp, this.half);
      const [r, g, b] = hot(h);
      return `<span class="b" data-w="${w}" style="background:rgb(${r} ${g} ${b} / ${(0.12 + 0.75 * h).toFixed(2)})">${text}</span>`;
    };
    const rows = s.statics
      .filter((st) => st.base < s.end)
      .map((st) => {
        let last = 0;
        let by = -1;
        for (let a = st.base & ~3; a < Math.min(st.base + st.size, s.end); a += 4) {
          const w = wordOf(a);
          if (s.words[w] > last) {
            last = s.words[w];
            by = s.words[w + 1];
          }
        }
        return { st, last, by };
      })
      .filter((row) => this.mark === null || !this.onlyMarked || row.last > this.mark);
    if (this.sort === "recent") rows.sort((a, b) => b.last - a.last);
    const lines = rows
      .map(({ st, last, by }) => {
        const shown = Math.min(st.size, this.open.has(st.base) ? 1024 : 16, s.end - st.base);
        let bytes = "";
        for (let a = st.base; a < st.base + shown; a++) {
          if (a > st.base && (a - st.base) % 16 === 0) bytes += `<br><span class="addr">${(a - st.base).toString(16).padStart(4, "0")}</span> `;
          bytes += byteHtml(a);
        }
        const more = st.size > shown ? `<span class="dim"> …${st.size - shown} more</span>` : "";
        const writer = by >= 0 ? s.writers[by]?.code : null;
        const when = last
          ? `<span class="t" data-stand="${last}" title="stand the board just after this write">${age(this.now - last)} ago</span>${writer ? ` <span class="dim">by ${e(writer)}</span>` : ""}`
          : `<span class="dim">never written</span>`;
        const near = this.focus !== null && this.focus >= st.base && this.focus < st.base + st.size;
        return `<div class="mem-static${near ? " near" : ""}${this.mark !== null && last > this.mark ? " since" : ""}" data-static="${st.base}"><div class="mem-static-head"><span class="name" title="${e(st.name)}">${e(st.stack ? st.name : shortName(st.name))}</span> <span class="dim">0x${st.base.toString(16)} · ${st.size} B</span> · ${when}</div><div class="mem-bytes">${bytes}${more}</div></div>`;
      })
      .join("");
    const owner = this.selected ? `<b>${e(this.selected)}</b>` : "";
    const sorts = ["address", "recent"]
      .map((sort) => `<button type="button" data-sort="${sort}"${sort === this.sort ? ' class="on"' : ""}>${sort === "address" ? "by address" : "latest written first"}</button>`)
      .join("");
    const marked = this.mark === null ? "" : `<button type="button" data-only${this.onlyMarked ? ' class="on"' : ""}>only written since the mark</button>`;
    const truncated = s.end < Math.max(...s.statics.map((st) => st.base + st.size)) ? `<div class="dim">the first ${INSPECTED / 1024} KiB shown</div>` : "";
    set(box, `<div class="mem-inspector-head">${owner} <span class="controls">${sorts}${marked}</span> <span class="dim mem-hover">&nbsp;</span></div>${lines || `<div class="dim">nothing to show</div>`}${truncated}`);
    box.onmouseover = (event) => {
      const byte = event.target.closest("[data-w]");
      const hover = box.querySelector(".mem-hover");
      if (!byte || !hover) return;
      const w = Number(byte.dataset.w);
      const writer = s.writers[s.words[w + 1]];
      hover.textContent = `this word: written ${age(this.now - s.words[w])} ago by ${writer?.code ?? "?"}`;
    };
    const near = box.querySelector(".mem-static.near");
    if (near && this.focusShown !== this.focus) {
      this.focusShown = this.focus;
      near.scrollIntoView({ block: "nearest" });
    }
  }

  clicked(event) {
    const t = (selector) => event.target.closest(selector);
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
      return this.drawInspector();
    }
    if (t("[data-mark]")) {
      this.mark = this.mark === null ? this.now : null;
      this.onlyMarked = false;
      this.draw();
      return this.drawInspector();
    }
    const owner = t("[data-owner]");
    if (owner) {
      this.selected = this.selected === owner.dataset.owner ? null : owner.dataset.owner;
      this.focus = null;
      this.inspected = null;
      this.draw();
      return this.inspect();
    }
    if (t("[data-sort]")) {
      this.sort = t("[data-sort]").dataset.sort;
      return this.drawInspector();
    }
    if (t("[data-only]")) {
      this.onlyMarked = !this.onlyMarked;
      return this.drawInspector();
    }
    const stand = t("[data-stand]");
    // The write was within this microsecond of the board's; its end is
    // just after it.
    if (stand) return this.stand(Number(stand.dataset.stand) * 1e6);
    const head = t(".mem-static-head");
    if (head) {
      const base = Number(head.parentElement.dataset.static);
      if (this.open.has(base)) this.open.delete(base);
      else this.open.add(base);
      return this.drawInspector();
    }
  }
}

/// How many cells of `view` are in `rect`.
function countIn(view, rect) {
  let n = 0;
  for (let y = rect.y; y < rect.y + rect.h; y++) for (let x = rect.x; x < rect.x + rect.w; x++) if (view.at[y * view.width + x] >= 0) n++;
  return n;
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
