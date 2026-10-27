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
    this.reset();
    this.image = null;
    this.owners = [];
    element.innerHTML = `<div class="mem-head"><span class="controls mem-halves"></span> <span class="controls"><button type="button" data-mark title="mark this instant: the cells written since are outlined, and the inspector can show only what was">mark</button></span> <span class="dim mem-note"></span></div>
      <div class="mem-maps"></div>
      <div class="mem-readout dim">&nbsp;</div>
      <div class="mem-owners"></div>
      <div class="mem-inspector"></div>`;
    element.onclick = (event) => this.clicked(event);
    this.whatis = new Map();
  }

  reset() {
    this.rams = null;
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
      this.owners = [];
      this.symbols = new Map();
      this.whatis.clear();
      this.tasks = null;
      this.read("board/tasks")
        .then((tasks) => {
          if (tasks?.error) return;
          this.tasks = tasks;
          this.owners = owners(tasks, this.colorOf);
          if (this.rams) this.rams.forEach((ram) => Object.assign(ram, cellOwners(ram.base, ram.view, this.owners)));
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
      this.inspected = null;
      this.poll(true);
    } else this.poll(false);
  }

  async poll(force) {
    const now = performance.now();
    if (this.busy || (!force && now - (this.polled ?? 0) < POLL_MS)) return;
    this.busy = true;
    this.polled = now;
    try {
      const since = this.rams ? this.now : 0;
      const answer = await this.read(`board/writes/${since}/${CELL}`);
      if (!answer?.rams) return;
      // The board went back — a reset, a seek: everything again.
      if (this.rams && answer.now < this.now) {
        this.rams = null;
        this.busy = false;
        return this.poll(true);
      }
      if (!this.rams) {
        this.rams = answer.rams.map((ram) => {
          const view = layout(ram.length, answer.cell);
          return { ...ram, view, stamps: new Uint32Array(view.cells), ...cellOwners(ram.base, view, this.owners) };
        });
      }
      const cells = answer.cells;
      for (let i = 0; i < cells.length; i += 3) this.rams[cells[i]].stamps[cells[i + 1]] = cells[i + 2];
      this.now = answer.now;
      this.draw();
      await this.inspect();
    } catch {
      // Nothing to read at this instant.
    } finally {
      this.busy = false;
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

  drawMaps() {
    const box = this.element.querySelector(".mem-maps");
    if (!this.rams) return;
    if (box.childElementCount !== this.rams.length) {
      box.innerHTML = this.rams
        .map((ram, n) => `<figure class="mem-ram"><figcaption><b>${this.escape(ram.name)}</b> <span class="dim">${(ram.length / 1024).toFixed(0)} KiB · 0x${ram.base.toString(16).padStart(8, "0")}</span></figcaption><canvas data-ram="${n}"></canvas></figure>`)
        .join("");
      for (const canvas of box.querySelectorAll("canvas")) this.equip(canvas);
    }
    const chosen = this.owners.findIndex((o) => o.name === this.selected);
    const colors = this.owners.map((o) => rgbOf(o.color));
    this.rams.forEach((ram, n) => {
      const canvas = box.querySelector(`canvas[data-ram="${n}"]`);
      const { view } = ram;
      const w = view.width * SCALE;
      const h = view.height * SCALE;
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
      }
      const g = canvas.getContext("2d");
      const image = g.createImageData(w, h);
      const px = image.data;
      const paint = (x, y, rgb) => {
        const i = 4 * (y * w + x);
        px[i] = rgb[0];
        px[i + 1] = rgb[1];
        px[i + 2] = rgb[2];
        px[i + 3] = 255;
      };
      for (let c = 0; c < view.cells; c++) {
        const owner = ram.owner[c];
        const stamp = ram.stamps[c];
        let rgb = BACKGROUND;
        if (owner >= 0) rgb = mix(BACKGROUND, colors[owner], ram.stack[c] ? 0.22 : 0.4);
        else if (stamp) rgb = [44, 48, 56];
        if (stamp && owner >= 0) rgb = mix(rgb, colors[owner], 0.15);
        if (stamp) {
          const h = heat(this.now - stamp, this.half);
          if (h > 0.02) rgb = mix(rgb, hot(h), Math.min(1, 0.25 + h));
        }
        if (chosen >= 0 && owner !== chosen) rgb = mix(BACKGROUND, rgb, 0.35);
        const x0 = view.place[2 * c] * SCALE;
        const y0 = view.place[2 * c + 1] * SCALE;
        const marked = this.mark !== null && stamp > this.mark;
        for (let dy = 0; dy < SCALE; dy++)
          for (let dx = 0; dx < SCALE; dx++) paint(x0 + dx, y0 + dy, marked && (dx === 0 || dy === 0) ? [80, 220, 255] : rgb);
      }
      // Where one owner's memory meets another's, a line.
      for (let y = 0; y < view.height; y++)
        for (let x = 0; x < view.width; x++) {
          const c = view.at[y * view.width + x];
          if (c < 0) continue;
          const right = x + 1 < view.width ? view.at[y * view.width + x + 1] : -1;
          const below = y + 1 < view.height ? view.at[(y + 1) * view.width + x] : -1;
          if (right >= 0 && ram.owner[right] !== ram.owner[c])
            for (let d = 0; d < SCALE; d++) paint(x * SCALE + SCALE - 1, y * SCALE + d, [0, 0, 0]);
          if (below >= 0 && ram.owner[below] !== ram.owner[c])
            for (let d = 0; d < SCALE; d++) paint(x * SCALE + d, y * SCALE + SCALE - 1, [0, 0, 0]);
        }
      g.putImageData(image, 0, 0);
      // Each owner's name, where most of its memory here is.
      const sums = new Map();
      for (let c = 0; c < view.cells; c++) {
        const owner = ram.owner[c];
        if (owner < 0) continue;
        const s = sums.get(owner) ?? { x: 0, y: 0, n: 0 };
        s.x += view.place[2 * c];
        s.y += view.place[2 * c + 1];
        s.n++;
        sums.set(owner, s);
      }
      g.font = "10px ui-monospace, monospace";
      g.textAlign = "center";
      g.textBaseline = "middle";
      g.lineWidth = 3;
      g.strokeStyle = "#000c";
      g.fillStyle = "#fff";
      for (const [owner, s] of sums) {
        if (s.n * SCALE * SCALE < 900) continue;
        const x = ((s.x / s.n) + 0.5) * SCALE;
        const y = ((s.y / s.n) + 0.5) * SCALE;
        g.strokeText(this.owners[owner].name, x, y);
        g.fillText(this.owners[owner].name, x, y);
      }
    });
  }

  /// The cell of a RAM's picture under the pointer.
  cellAt(canvas, event) {
    const ram = this.rams?.[Number(canvas.dataset.ram)];
    if (!ram) return null;
    const box = canvas.getBoundingClientRect();
    const x = Math.floor(((event.clientX - box.left) / box.width) * ram.view.width);
    const y = Math.floor(((event.clientY - box.top) / box.height) * ram.view.height);
    if (x < 0 || y < 0 || x >= ram.view.width || y >= ram.view.height) return null;
    const c = ram.view.at[y * ram.view.width + x];
    return c < 0 ? null : { ram, c, address: ram.base + c * CELL };
  }

  equip(canvas) {
    const readout = () => this.element.querySelector(".mem-readout");
    canvas.onmousemove = (event) => {
      const hit = this.cellAt(canvas, event);
      if (!hit) return (readout().textContent = " ");
      const { ram, c, address } = hit;
      const owner = this.owners[ram.owner[c]];
      const stamp = ram.stamps[c];
      const what = this.named(address);
      const written = stamp ? `written ${age(this.now - stamp)} ago` : "never written";
      readout().textContent = `0x${address.toString(16).padStart(8, "0")}–0x${(address + CELL - 1).toString(16)} · ${what ?? (owner ? owner.name : ram.name)} · ${written}`;
    };
    canvas.onmouseleave = () => (readout().textContent = " ");
    canvas.onclick = (event) => {
      const hit = this.cellAt(canvas, event);
      if (!hit) return;
      const owner = this.owners[hit.ram.owner[hit.c]];
      this.selected = owner?.name ?? null;
      this.focus = hit.address;
      this.inspected = null;
      this.draw();
      this.inspect();
    };
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
