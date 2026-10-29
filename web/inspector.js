// The memory panel's inspector: what a task's memory is, as its DWARF says.
// See `memory.js`, which owns it and reads the bytes it shows, and
// `debuginfo.js`, which decodes them.
//
// For the task chosen, five views:
//
//   variables   its statics as typed trees: each field's value, how lately
//               it was written and by which source line, a ring buffer as
//               its log; and for any field, its history (every write to it
//               over a window, from a re-execution watching it) and why it
//               holds what it does (each copy back to where the value came
//               in from outside, from a re-execution following values).
//   stack       its stack stepped back frame by frame, each frame's source
//               line, the calls inlined there, and its variables.
//   writers     its memory by the source line that last wrote it.
//   audit       who wrote what across the whole board over a window, and
//               any code that wrote on from one variable into the next.
//   changes     every field of every task that changed since the mark.
//
// Source opens beside any of them at a line, each line coloured by how
// much of the task's memory it last wrote; a click on one outlines that
// memory in the pictures.

import { Types, changes, decode, guess, hexBytes, ringbuf, scheduling, score, walk } from "./debuginfo.js";
import { progressHtml } from "./progress.js";
import { addressOf, clutBytes, colour, firstView, layerFor, pixelOf, shapeOf } from "./views.js";
import { HOPS, byteOf, folded, follow, functionOf, leafText, next, ownerOf, placeOwner, span, stepsOf, summary } from "./trail.js";

const MODES = [
  ["variables", "variables"],
  ["stack", "stack"],
  ["writers", "writers"],
  ["audit", "audit"],
  ["changes", "changes since the mark"],
];
/// How far back a history or an audit looks, in picoseconds of the board's.
const BACKS = [
  [1e11, "100 ms"],
  [1e12, "1 s"],
  [1e13, "10 s"],
  [1e18, "since it started"],
];
const CONTEXT = 12; // lines of source either side of the one shown
/// Types that only wrap another, whose fields a name leaves out.
const WRAPPERS = /^(core::mem::(maybe_uninit::)?|core::cell::|)(MaybeUninit|ManuallyDrop|UnsafeCell|StaticCell|Cell|RefCell|SyncUnsafeCell)</;
const SERVED = 4096; // bytes a read of memory answers with
const LOG = 16; // ring buffer entries shown, the latest

export class Inspector {
  /// `view` is the memory panel's `MemoryView`.
  constructor(view, box) {
    this.view = view;
    this.box = box;
    this.mode = "variables";
    this.back = 1e12;
    this.open = new Set(); // node keys shown whole
    this.reset();
  }

  /// Another image: nothing kept is about it.
  reset() {
    this.typed = new Map(); // owner → { statics, types } | { error }, or a promise of one
    this.stacks = new Map(); // owner → { answer, last }
    this.writers = new Map(); // owner → { answer, read }
    this.files = new Map(); // path → text, or a promise of it
    this.source = null; // { file, line, whole }
    this.history = null; // { key, address, size, type, owner, answer | error }
    this.trail = null; // { address, label, hops, end, running, error }: see trail.js
    this.windows = new Map(); // the stories' windows run so far: see trail.js
    this.stories = []; // every story asked for, oldest first, to go back and forth among
    this.lit = null; // the step the pointer is on: its bytes outlined in the pictures
    this.viewing = null; // { key, address, size, name, mode, width, offset }: bytes seen in their own form
    this.reach = null; // { address, label, from, window, running, progress, answer | error }: where a value went
    this.scanned = null; // what the display controller scans out: `board/scanned`'s layers
    this.audit = null; // { back, answer | error | running }
    this.snapshot = null; // { stamp, owners: Map(owner → { statics, bytes }) }
    this.compared = null;
    this.schedules = [];
  }

  get e() {
    return this.view.escape;
  }

  // ---- what is read ----------------------------------------------------------

  /// An owner's typed statics and their types: `board/typed`, read once.
  async typedOf(owner) {
    let entry = this.typed.get(owner);
    if (!entry) {
      entry = this.view
        .read(`board/typed/${owner}`)
        .then((answer) => (answer?.statics ? { statics: answer.statics, types: new Types().add(answer.types) } : { error: answer?.error ?? "no DWARF" }))
        .catch((why) => ({ error: String(why) }));
      this.typed.set(owner, entry);
      entry.then((done) => this.typed.set(owner, done));
    }
    return entry;
  }

  /// The owner's typed statics if they have been read, without waiting.
  typedNow(owner) {
    const entry = this.typed.get(owner);
    return entry && !(entry instanceof Promise) ? entry : null;
  }

  /// `length` bytes at `base`, read now, as a function of an address.
  async bytesAt(base, length) {
    let hex = "";
    for (let at = base; at < base + length; at += SERVED) {
      const answer = await this.view.read(`board/memory/${at}/${Math.min(SERVED, base + length - at)}`);
      hex += answer?.hex ?? "";
    }
    return hexBytes(hex, base);
  }

  /// The source file at `path`, as the DWARF names it.
  fileOf(path) {
    if (!this.files.has(path)) {
      const reading = this.view
        .read(`board/file${path}`)
        .then((answer) => answer?.text ?? null)
        .catch(() => null)
        .then((text) => {
          this.files.set(path, text);
          this.draw(true);
          return text;
        });
      this.files.set(path, reading);
    }
    const text = this.files.get(path);
    return text instanceof Promise ? undefined : text;
  }

  /// Brought up to date as the panel polls: the stack when it was written
  /// since it was read, the writers now and then, and what each task is
  /// doing.
  async refresh() {
    const owner = this.view.selected;
    if (!owner) return;
    if (this.mode === "stack") {
      const stack = this.view.stackOf(owner);
      const last = stack ? this.view.lastWrite(stack.base, stack.base + stack.size) : 0;
      const held = this.stacks.get(owner);
      if (!held || held.last !== last || held.instant !== this.view.instant) {
        const generation = this.view.generation;
        const answer = await this.view.read(`board/stack/${owner}`).catch((why) => ({ error: String(why) }));
        if (generation !== this.view.generation) return;
        const types = (await this.typedOf(owner)).types ?? new Types();
        types.add(answer?.types);
        this.stacks.set(owner, { answer, last, instant: this.view.instant, types });
        this.draw(true);
      }
    }
    if (this.mode === "writers" || this.source) {
      const held = this.writers.get(owner);
      if (!held || performance.now() - held.read > 1000) await this.readWriters(owner);
    }
  }

  async readWriters(owner) {
    const answer = await this.view.read(`board/writers/${owner}`).catch(() => null);
    if (!answer?.writers) return;
    // By source line: the innermost line of each writer's.
    const lines = new Map();
    for (const w of answer.writers) {
      const line = w.writer.lines?.[0];
      const key = line?.file ? `${line.file}:${line.line}` : w.writer.code;
      const entry = lines.get(key) ?? { key, line: line ?? null, code: w.writer.code, words: 0, latest: 0, runs: [] };
      entry.words += w.words;
      entry.latest = Math.max(entry.latest, w.latest);
      entry.runs.push(...w.runs);
      lines.set(key, entry);
    }
    this.writers.set(owner, { answer, lines, read: performance.now() });
    this.draw(true);
  }

  /// What each task is doing, from the kernel's task table decoded by its
  /// own types: `[{ text, on }]` by task index.
  async readSchedules() {
    const kernel = await this.typedOf("kernel");
    const table = kernel.statics?.find((s) => /HUBRIS_TASK_TABLE_SPACE$/.test(s.name));
    if (!table) return;
    const byte = await this.bytesAt(table.address, table.size).catch(() => null);
    if (!byte) return;
    const tree = decode(kernel.types, table.type, table.address, byte, "tasks");
    this.schedules = (tree.children ?? []).map((entry) => {
      for (const [node] of walk(entry)) {
        const state = node.children?.find((c) => c.name === "state");
        if (state) return scheduling(state);
      }
      return null;
    });
  }

  // ---- what is asked of a re-execution ----------------------------------------

  /// Every write to a field over the window, from a re-execution watching it.
  async readHistory(target) {
    this.history = { ...target, back: this.back, running: true };
    this.draw(true);
    const history = this.history;
    try {
      history.answer = await this.view.query({ watch: `range ${target.address} ${Math.max(1, target.size)}`, back: this.back }, "board/watched", (p) => this.progressed("history", history, p));
    } catch (why) {
      history.error = String(why);
    }
    history.running = false;
    if (this.history === history) this.draw(true);
  }

  /// Where the value at `address` came from, back through every write:
  /// see `trail.js`. From `from`, `{ at, time }`, if given; going on with
  /// the story told so far if `more`.
  async readTrail(address, label, from = null, more = false) {
    // From the write the inspector shows, not one made since: the board
    // runs on while it is looked at.
    if (!from) {
      const [stamp, writer] = this.view.latestIn(address, address + 1);
      if (stamp && this.view.inspected?.writers[writer]?.pc !== undefined) from = { at: address, time: stamp * 1e6 };
    }
    let trail = this.trail;
    if (!more) {
      trail = { address, label, hops: [], asked: from?.time ?? null };
      // A new story drops the ones gone back from, as a browser does.
      this.stories = [...this.stories.slice(0, this.stories.indexOf(this.trail) + 1), trail];
    }
    trail.from = from;
    trail.end = null;
    trail.error = null;
    trail.stopped = false;
    this.trail = trail;
    const wanted = () => this.trail === trail && !trail.stopped;
    const update = () => wanted() && this.draw(true);
    trail.running = { at: from?.at ?? address, time: from?.time ?? null, window: 0 };
    update();
    try {
      const query = (options, path) => this.view.query(options, path, (p) => wanted() && this.progressed("trail", trail.running, p));
      await follow(trail, { read: (path) => this.view.read(path), query, update, wanted, cache: this.windows });
    } catch (why) {
      trail.error = String(why);
    }
    // Stopped: where it would have gone on from, to go on from there.
    if (trail.stopped && !trail.end) {
      const after = next(trail.hops.at(-1)?.steps.at(-1));
      trail.end = after.at !== undefined ? { kind: "stopped", at: after.at, time: after.time } : { kind: "stopped" };
    }
    trail.running = null;
    if (this.trail === trail) this.draw(true);
  }

  /// Where the byte at `address` went after `from` (picoseconds of the
  /// board's), over `window`: a re-execution following it alone forward,
  /// and every place in memory that holds something made of it at the end.
  async readReach(address, label, from, window = this.reach?.window ?? 1e11) {
    const reach = { address, label, from, window, running: true };
    this.reach = reach;
    this.draw(true);
    try {
      reach.answer = await this.view.query({ time: from + window, follow: { forward: { from, address, length: 1 } } }, "board/reached", (p) => this.progressed("reach", reach, p));
    } catch (why) {
      reach.error = String(why);
    }
    reach.running = false;
    if (this.reach === reach) {
      this.view.setHighlight(this.reachRuns());
      this.draw(true);
    }
  }

  /// The places a value reached, as runs to outline: `[address, length]`.
  reachRuns() {
    return (this.reach?.answer?.shown ?? []).map((r) => [r.addr, r.length]);
  }

  reachHtml() {
    const e = this.e;
    const r = this.reach;
    const hex = (n) => `0x${(n >>> 0).toString(16)}`;
    const windows = [
      [1e9, "1 ms"],
      [1e10, "10 ms"],
      [1e11, "100 ms"],
      [1e12, "1 s"],
    ]
      .map(([w, label]) => `<button type="button" data-reach-window="${w}"${w === r.window ? ' class="on"' : ""}>${label}</button>`)
      .join("");
    const head = `<div class="story-head"><b>where ${e(r.label ?? hex(r.address))} went</b> <span class="dim">over the</span> <span class="controls">${windows}</span> <span class="dim">after t=${(r.from / 1e9).toFixed(3)} ms</span> <span class="controls"><button type="button" data-close="reach">close</button></span></div>`;
    let body;
    if (r.running) body = `<div class="dim">following it forward, and everything made of it</div><div data-progress="reach">${progressHtml(r.progress)}</div>`;
    else if (r.error) body = `<div class="dim">${e(r.error)}</div>`;
    else if (!r.answer?.following) body = `<div class="dim">nothing followed: no board at the checkpoint it began from</div>`;
    else {
      const a = r.answer;
      const short = (where) => (where ?? "").replace(/ \((.*)\)$/, "").replace(/\b(\w+::)+/g, "");
      const rows = a.shown
        .map((run) => {
          const owner = /^([\w-]+) (\.[\w.]+|stack)/.exec(run.where ?? "")?.[1];
          const line = run.lines?.[0];
          const by = line?.file ? `${e((line.function ?? "").replace(/^.*::/, ""))} ${this.lineLink(line)}` : e(run.code ?? "");
          return `<li data-lit="${run.addr}"><a data-goto="${run.addr}">${e(short(run.where).replace(/^[\w-]+ [.\w]+: /, ""))}</a>${owner ? ` ${this.ownerChip(owner)}` : ""}${run.length > 1 ? ` <span class="dim">${run.length} bytes</span>` : ""} <span class="dim">by</span> ${by}</li>`;
        })
        .join("");
      const owners = [...new Set(a.shown.map((run) => /^([\w-]+) (\.[\w.]+|stack)/.exec(run.where ?? "")?.[1]).filter(Boolean))];
      const said = a.bytes ? `It reached ${a.bytes} byte${a.bytes === 1 ? "" : "s"} in ${a.runs} place${a.runs === 1 ? "" : "s"}${owners.length ? `, in ${owners.join(", ")}` : ""}, still there at the end of the window.` : "Nothing holds anything made of it at the end of the window: what it went into was overwritten, or it went nowhere.";
      body = `<div class="story-summary">${e(said)}</div>${rows ? `<ol class="story-steps">${rows}</ol>` : ""}${a.runs > a.shown.length ? `<div class="dim">and ${a.runs - a.shown.length} more places</div>` : ""}`;
    }
    return `<div class="mem-story mem-reach">${head}${body}</div>`;
  }

  async runAudit() {
    this.audit = { back: this.back, running: true };
    this.draw(true);
    const audit = this.audit;
    try {
      audit.answer = await this.view.query({ watch: "audit", back: this.back }, "board/audit", (p) => this.progressed("audit", audit, p));
    } catch (why) {
      audit.error = String(why);
    }
    audit.running = false;
    if (this.audit === audit) this.draw(true);
  }

  /// How far the re-execution behind `kind` — a history, a story, an
  /// audit — has got: kept on `on`, and its bar redrawn in place.
  progressed(kind, on, report) {
    if (!on) return;
    on.progress = report;
    const box = this.box.querySelector(`[data-progress="${kind}"]`);
    if (box) box.innerHTML = progressHtml(report);
  }

  /// Every task's typed statics' bytes, as they are now.
  async capture() {
    const owners = new Map();
    for (const o of this.view.owners) {
      const typed = await this.typedOf(o.name);
      if (!typed.statics?.length) continue;
      const statics = typed.statics.filter((s) => s.size <= 65536);
      const bytes = new Map();
      for (const s of statics) bytes.set(s.address, await this.bytesAt(s.address, s.size).catch(() => () => null));
      owners.set(o.name, { statics, bytes, types: typed.types });
    }
    return owners;
  }

  async takeSnapshot() {
    this.snapshot = { stamp: this.view.now, owners: null };
    this.compared = null;
    this.snapshot.owners = await this.capture();
    this.draw(true);
  }

  async compare() {
    if (!this.snapshot?.owners) return;
    this.compared = { running: true };
    this.draw(true);
    const now = await this.capture();
    const out = [];
    for (const [owner, then] of this.snapshot.owners) {
      const current = now.get(owner);
      if (!current) continue;
      for (const s of then.statics) {
        const before = decode(then.types, s.type, s.address, then.bytes.get(s.address), s.name);
        const after = decode(current.types, s.type, s.address, current.bytes.get(s.address), s.name);
        const found = changes(before, after);
        const logs = [ringbuf(before), ringbuf(after)];
        const added = logs[1] ? Math.max(0, logs[1].log.length - (logs[0]?.log.length ?? 0)) : 0;
        if (found.length) out.push({ owner, name: s.name, found, ring: logs[1] ? { added, last: logs[1].log.slice(-3) } : null });
      }
    }
    this.compared = { running: false, out, at: this.view.now };
    this.draw(true);
  }

  // ---- what an address is ------------------------------------------------------

  /// What an address is, for a pointer's value and the guesser: a static
  /// by name, an owner's code or RAM.
  whereOf() {
    const statics = [];
    for (const o of this.view.owners) {
      const typed = this.typedNow(o.name);
      for (const s of typed?.statics ?? []) statics.push(s);
    }
    statics.sort((a, b) => a.address - b.address);
    const code = [];
    for (const t of [this.view.tasks?.kernel, ...(this.view.tasks?.tasks ?? [])].filter(Boolean))
      for (const s of t.sections) if (s.executable) code.push({ base: s.base, end: s.base + s.size, name: t.name });
    const stacks = this.view.owners.filter((o) => o.stack).map((o) => ({ base: o.stack.base, end: o.stack.base + o.stack.size, name: o.name }));
    return (address) => {
      for (const c of code) if (address >= c.base && address < c.end) return { code: `${c.name} code` };
      for (const st of stacks) if (address >= st.base && address < st.end) return { data: `${st.name}'s stack` };
      let lo = 0;
      let hi = statics.length;
      while (lo < hi) {
        const mid = (lo + hi) >> 1;
        if (statics[mid].address <= address) lo = mid + 1;
        else hi = mid;
      }
      const s = statics[lo - 1];
      if (s && address < s.address + s.size) return { data: `${s.name.split("::").pop()}${address > s.address ? ` + 0x${(address - s.address).toString(16)}` : ""}` };
      const owner = this.view.owners.find((o) => o.ranges.some((r) => address >= r.base && address < r.end));
      return owner ? { data: `${owner.name}'s memory` } : null;
    };
  }

  // ---- drawing ------------------------------------------------------------------

  /// Rows made again only when what they are changes; otherwise each
  /// heat and age brought up to date in place.
  draw(force = false) {
    const box = this.box;
    const shape = [this.view.inspected?.id, this.mode, this.view.selected, this.view.focus, [...this.open].join(","), this.source && JSON.stringify(this.source), this.back].join("|");
    if (!force && box.shape === shape && box.firstChild) return this.warm();
    box.shape = shape;
    const e = this.e;
    const owner = this.view.selected;
    const tabs = MODES.map(([mode, label]) => `<button type="button" data-mode="${mode}"${mode === this.mode ? ' class="on"' : ""}${mode === "changes" && this.view.mark === null ? ' disabled title="set the mark first"' : ""}>${label}</button>`).join("");
    const index = this.view.owners.find((o) => o.name === owner)?.index;
    const doing = index !== undefined && index !== null ? this.schedules[index] : null;
    const state = doing ? ` <span class="dim">· ${e(this.scheduleText(doing))}</span>` : "";
    const head = `<div class="mem-inspector-head">${owner ? `<b>${e(owner)}</b>${state}` : `<span class="dim">no task chosen</span>`} <span class="controls">${tabs}</span> <span class="dim mem-hover">&nbsp;</span></div>`;
    let body = "";
    if (this.mode === "audit") body = this.auditHtml();
    else if (this.mode === "changes") body = this.changesHtml();
    else if (!owner && this.view.focus === null) body = `<div class="dim">click a task, or a place in the pictures, to see what is there</div>`;
    else if (this.mode === "stack") body = this.stackHtml(owner);
    else if (this.mode === "writers") body = this.writersHtml(owner);
    else body = this.variablesHtml(owner);
    const story = (this.trail ? this.trailHtml() : "") + (this.reach ? this.reachHtml() : "");
    box.innerHTML = `${head}${story}<div class="mem-views"><div class="mem-view">${body}</div>${this.sourceHtml()}</div>`;
    box.onmouseover = (event) => this.hovered(event);
    box.onchange = (event) => {
      if (!event.target.matches("[data-view-width]") || !this.viewing) return;
      this.viewing.width = Number(event.target.value) || null;
      this.draw(true);
    };
    this.warm();
    const near = box.querySelector(".near");
    if (near && this.focusShown !== this.view.focus) {
      this.focusShown = this.view.focus;
      near.scrollIntoView({ block: "nearest" });
    }
    this.view.setArrows([...(this.arrows ?? []), ...this.storyArrows()]);
    this.paintPicture();
  }

  scheduleText(doing) {
    const other = doing.on !== null && doing.on !== undefined ? this.view.owners.find((o) => o.index === doing.on)?.name : null;
    return other ? `${doing.text} from ${other}` : doing.text;
  }

  /// Each heat swatch and each age, as of now.
  warm() {
    for (const swatch of this.box.querySelectorAll("[data-heat]")) {
      const [from, to] = swatch.dataset.heat.split(",").map(Number);
      const last = this.view.stampOf(from, to);
      const h = last ? this.view.heatOf(last) : 0;
      swatch.style.background = this.view.hotColour(h, 0.15 + 0.8 * h);
      swatch.title = last ? `written ${this.view.ago(last)} ago` : "never written";
    }
    for (const line of this.box.querySelectorAll("[data-heat-stamp]")) {
      const h = this.view.heatOf(Number(line.dataset.heatStamp));
      line.style.background = this.view.hotColour(h, 0.25 + 0.75 * h);
    }
    for (const when of this.box.querySelectorAll("[data-ago]")) when.textContent = `${this.view.ago(Number(when.dataset.ago))} ago`;
  }

  /// Where a source line is, as a link that opens it.
  lineLink(line, label = null) {
    if (!line?.file) return "";
    const short = line.file.split("/").slice(-2).join("/");
    return `<a class="src" data-src="${this.e(line.file)}" data-line="${line.line}" title="${this.e(`${line.function ?? ""} ${line.file}:${line.line}`)}">${this.e(label ?? `${short}:${line.line}`)}</a>`;
  }

  /// A writer as the inspector names it: its function and line.
  writerHtml(writer) {
    if (!writer) return "";
    const line = writer.lines?.[0];
    const fn = (line?.function ?? writer.code ?? "").replace(/^.*::/, "");
    return line?.file ? `${this.e(fn)} ${this.lineLink(line)}` : this.e(writer.code ?? "?");
  }

  /// A decoded value's rows: itself, and if open, its parts.
  nodeHtml(node, key, depth, actions, latest) {
    const e = this.e;
    const open = this.open.has(key);
    const parts = node.children?.length;
    const toggle = parts ? `<span class="tw" data-toggle="${e(key)}">${open ? "▾" : "▸"}</span>` : `<span class="tw"></span>`;
    const heat = node.address !== null && node.size && actions ? `<span class="heat" data-heat="${node.address},${node.address + node.size}"></span>` : "";
    const by = latest ? latest(node) : "";
    const act = actions
      ? `<span class="acts"><a data-history="${e(key)}" title="every write to it over the window, from a re-execution watching it">history</a> <a data-why="${e(key)}" title="where its value came from: each copy and each write back to where it came in, from re-executions following every value">why</a>${node.bytes && node.size >= 16 ? ` <a data-view="${e(key)}" title="its bytes as a picture, hex or text: any of them a place to ask why">view</a>` : ""}</span>`
      : "";
    const near = this.view.focus !== null && depth === 0 && this.view.focus >= node.address && this.view.focus < node.address + node.size;
    let html = `<div class="mem-node${near ? " near" : ""}${node.unknown ? " unknown" : ""}" style="padding-left:${depth * 1.2}em" data-key="${e(key)}">${toggle}${heat}<span class="name">${e(depth ? node.name : node.name.split("::").slice(-2).join("::"))}</span><span class="ty">: ${e(node.typeName)}</span> = <span class="val">${e(node.value)}</span>${by}${act}</div>`;
    if (this.history?.key === key) html += this.historyHtml(node);
    if (this.viewing?.key === key) html += this.viewHtml();
    if (open && parts) {
      for (const child of node.children) html += this.nodeHtml(child, `${key}/${child.name}`, depth + 1, actions, latest);
      if (node.more) html += `<div class="dim" style="padding-left:${(depth + 1) * 1.2}em">…${node.more} more of ${node.more + node.children.length}</div>`;
    }
    return html;
  }

  variablesHtml(owner) {
    const e = this.e;
    const s = this.view.inspected;
    const typed = owner ? this.typedNow(owner) : null;
    if (!s) return `<div class="dim">reading…</div>`;
    const byte = (address) => this.view.byteAt(address);
    const where = this.whereOf();
    const describe = (address) => where(address)?.data ?? where(address)?.code ?? null;
    const latest = (node) => {
      const [stamp, writer] = this.view.latestIn(node.address, node.address + node.size);
      return stamp ? ` <span class="by dim"><span data-ago="${stamp}"></span> by ${this.writerHtml(s.writers[writer])} <a class="stand" data-stand="${stamp}" title="stand the board just after this write">⟲</a></span>` : ` <span class="by dim">never written</span>`;
    };
    const trees = [];
    this.arrows = [];
    let html = "";
    if (typed?.error) html += `<div class="dim">no types for ${e(owner)}: ${e(typed.error)}; its bytes as they are</div>`;
    for (const st of s.statics) {
      if (st.type === undefined || !typed?.types) {
        html += this.view.rawStaticHtml(st);
        continue;
      }
      const key = `${st.base}`;
      const tree = decode(typed.types, st.type, st.base, byte, st.name, describe);
      trees.push(tree);
      for (const [node] of walk(tree)) if (node.target && node.kind === "pointer") this.arrows.push([node.address, node.target]);
      const ring = ringbuf(tree);
      html += `<div class="mem-static">${this.nodeHtml(tree, key, 0, true, latest)}`;
      if (ring) {
        const rows = ring.log
          .slice(-LOG)
          .reverse()
          .map((entry) => `<tr><td class="dim">${entry.count > 1 ? `×${entry.count}` : ""}</td><td>${e(entry.payload)}</td><td class="dim">line ${entry.line}</td></tr>`)
          .join("");
        html += `<div class="mem-ring"><span class="dim">its log, latest first (${ring.log.length} of ${ring.entries} slots used)</span><table>${rows || `<tr><td class="dim">empty</td></tr>`}</table></div>`;
      }
      html += `</div>`;
    }
    for (const st of s.around ?? []) html += this.aroundHtml(st, where);
    const focus = this.view.focus;
    if (focus !== null) html = `<div class="mem-focus">0x${focus.toString(16)} <span class="dim">${e(describe(focus) ?? "")}</span> <a data-trail="${focus}" title="where the value there came from, write by write">why</a></div>${html}`;
    // The guesser, scored against what the DWARF says these hold.
    if (trees.length && typed?.types) {
      const { total, right, misses } = score(typed.types, trees, byte, where);
      if (total) {
        const missed = misses.map((m) => `${m.path.join(".")}: a ${m.truth}, guessed ${m.guess.kind} (${m.guess.label})`).join("\n");
        html = `<div class="dim mem-score" title="${e(missed || "no misses")}">guessing at words alone, without the DWARF, would get ${right} of the ${total} words here whose kind it knows (${Math.round((100 * right) / total)}%)${misses.length ? " · misses on hover" : ""}</div>${html}`;
      }
    }
    if (s.cut) html += `<div class="dim">the first ${this.view.inspectedMost / 1024} KiB of its statics shown</div>`;
    return html || `<div class="dim">nothing to show</div>`;
  }

  /// Memory nobody's DWARF describes, a word a row, each guessed at.
  aroundHtml(st, where) {
    const e = this.e;
    const rows = [];
    for (let a = st.base; a < st.base + st.size; a += 4) {
      const bytes = [0, 1, 2, 3].map((i) => this.view.byteAt(a + i));
      if (bytes.some((b) => b === null)) continue;
      const word = (bytes[0] | (bytes[1] << 8) | (bytes[2] << 16) | (bytes[3] << 24)) >>> 0;
      const g = guess(word, where);
      const [stamp, writer] = this.view.latestIn(a, a + 4);
      const near = this.view.focus !== null && this.view.focus >= a && this.view.focus < a + 4;
      rows.push(
        `<tr class="${near ? "near" : ""}"><td class="dim">0x${a.toString(16)}</td><td><span class="heat" data-heat="${a},${a + 4}"></span>${word.toString(16).padStart(8, "0")}</td><td class="g-${g.kind}">${e(g.kind)}</td><td>${e(g.label)}</td><td class="dim">${stamp ? `<span data-ago="${stamp}"></span> by ${this.writerHtml(this.view.inspected.writers[writer])} <a data-trail="${a}">why</a>` : ""}</td></tr>`,
      );
    }
    return `<div class="mem-static"><div class="mem-static-head"><b>${e(st.name)}</b> <span class="dim">nothing's DWARF says what this is: each word guessed at alone</span></div><table class="mem-words">${rows.join("")}</table></div>`;
  }

  stackHtml(owner) {
    const e = this.e;
    const held = this.stacks.get(owner);
    if (!held) return `<div class="dim">stepping its stack back…</div>`;
    const { answer, types } = held;
    if (answer?.error) return `<div class="dim">${e(answer.error)}</div>`;
    if (!answer?.frames?.length) return `<div class="dim">${answer?.running === false ? "nothing to step back: it has never run, or (the kernel) it is not running now" : "no frames"}</div>`;
    const where = this.whereOf();
    const describe = (address) => where(address)?.data ?? where(address)?.code ?? null;
    const frames = answer.frames.map((frame, n) => {
      const [inner, ...outer] = frame.lines;
      const own = frame.lines.at(-1);
      const title = own?.function ?? frame.code;
      const chain = frame.lines.length > 1 ? `<div class="dim inl">${frame.lines.slice(0, -1).map((l) => `${e((l.function ?? "?").replace(/<.*>/, "<…>"))} ${this.lineLink(l)}`).join(" ← ")} <span>inlined here</span></div>` : "";
      const at = inner ?? own;
      // Variables by the function, inlined or not, they belong to.
      const groups = new Map();
      frame.variables.forEach((v, i) => {
        const list = groups.get(v.function) ?? [];
        list.push([v, i]);
        groups.set(v.function, list);
      });
      let vars = "";
      for (const [fn, list] of groups) {
        vars += `<div class="dim fn">${e(fn.replace(/<.*>/, "<…>"))}</div>`;
        for (const [v, i] of list) {
          const base = v.address ?? 0;
          const node = decode(types, v.type, base, hexBytes(v.hex, base), `${v.parameter ? "arg " : ""}${v.name}`, describe);
          if (v.address === null) node.address = null;
          vars += this.nodeHtml(node, `stack/${n}/${i}`, 1, v.address !== null, null);
        }
      }
      const where = at ? this.lineLink(at) : "";
      return `<div class="mem-frame-row"><div><b>#${n}</b> ${e(title.replace(/<.*>/, "<…>"))} ${where} <span class="dim">sp 0x${(frame.sp ?? 0).toString(16)}</span></div>${chain}${vars}</div>`;
    });
    return `<div class="dim">${answer.running ? "running now: its registers are the core's" : "stopped: stepped back from the registers the kernel saved for it"}</div>${frames.join("")}`;
  }

  writersHtml(owner) {
    const e = this.e;
    const held = this.writers.get(owner);
    if (!held) return `<div class="dim">reading who last wrote its memory…</div>`;
    const rows = [...held.lines.values()]
      .sort((a, b) => b.words - a.words)
      .map((w) => `<tr data-runs="${e(JSON.stringify(w.runs))}"><td>${w.line ? `${e((w.line.function ?? "").replace(/^.*::/, ""))} ${this.lineLink(w.line)}` : e(w.code)}</td><td class="num">${w.words * 4} B</td><td class="dim"><span data-ago="${w.latest}"></span></td><td><a data-outline title="outline this memory in the pictures">outline</a></td></tr>`)
      .join("");
    return `<div class="dim">its memory by the source line that last wrote each word: most first</div><table class="mem-table">${rows}</table>`;
  }

  backsHtml() {
    return BACKS.map(([ps, label]) => `<button type="button" data-back="${ps}"${ps === this.back ? ' class="on"' : ""}>${label}</button>`).join("");
  }

  historyHtml(node) {
    const e = this.e;
    const h = this.history;
    const head = `<div class="mem-history"><div class="dim">every write to ${e(node.name)} over the last <span class="controls">${this.backsHtml()}</span> <a data-close="history">close</a></div>`;
    if (h.running) return `${head}<div class="dim">running the board again to here, watching every write to it</div><div data-progress="history">${progressHtml(h.progress)}</div></div>`;
    if (h.error) return `${head}<div class="dim">${e(h.error)}</div></div>`;
    const a = h.answer;
    if (!a?.watching) return `${head}<div class="dim">nothing watched: no board at the checkpoint it began from</div></div>`;
    if (!a.writes.length) return `${head}<div class="dim">not written in that window</div></div>`;
    const now = a.now_ns;
    const first = a.writes[0][0];
    const span = Math.max(1, now - first);
    const ticks = a.writes.map(([t]) => `<span style="left:${((100 * (t - first)) / span).toFixed(2)}%"></span>`).join("");
    const types = this.typedNow(h.owner)?.types ?? this.stacks.get(h.owner)?.types;
    const rows = a.writes
      .slice(-40)
      .reverse()
      .map(([t, w, address, size, value]) => {
        let shown = `${size} B at +${address - h.address}: 0x${(value >>> 0).toString(16)}`;
        if (types && address === h.address && size === h.size && size <= 4) {
          const b = [value & 0xff, (value >>> 8) & 0xff, (value >>> 16) & 0xff, value >>> 24];
          shown = decode(types, h.type, 0, (x) => b[x] ?? null).value;
        }
        const stamp = Math.floor(t / 1000) + 1;
        return `<tr><td class="dim"><a data-stand="${stamp}" title="stand the board just after this write">${((now - t) / 1e6).toFixed(3)} ms ago</a></td><td>${e(shown)}</td><td class="dim">by ${this.writerHtml(a.writers[w])}</td></tr>`;
      })
      .join("");
    const dropped = a.dropped ? `<div class="dim">and ${a.dropped} more not kept</div>` : "";
    return `${head}<div class="mem-ticks" title="each write, across the window">${ticks}</div><table class="mem-table">${rows}</table>${dropped}</div>`;
  }

  /// A value's story, latest first: each window's history, the write that
  /// leads to the next, and where it came in.
  trailHtml() {
    const e = this.e;
    const t = this.trail;
    const hex = (n) => `0x${(n >>> 0).toString(16)}`;
    const byte = (n) => `0x${n.toString(16).padStart(2, "0")}`;
    // A place: its static and offset, whose memory it is beside it; the
    // memory's own name on hover.
    const short = (where) => (where ?? "").replace(/^[\w-]+ [.\w]+: /, "").replace(/ \((.*)\)$/, "").replace(/\b(\w+::)+/g, "");
    const place = (step) => {
      const address = step.byte ?? step.addr;
      const owner = placeOwner(step);
      return `<a data-goto="${address}" title="${e(`${hex(address)} · ${step.where ?? ""}`)}">${e(short(step.where) || hex(address))}</a>${owner ? ` ${this.ownerChip(owner)}` : ""}`;
    };
    const by = (step) => {
      const line = step.lines?.[0];
      const owner = ownerOf(step);
      return `${e(functionOf(step))}${line?.file ? ` ${this.lineLink(line)}` : ""}${owner && owner !== placeOwner(step) ? ` <span class="dim">in</span> ${this.ownerChip(owner)}` : ""}`;
    };
    // Each step's time, as how long before the one above it.
    let above = null;
    const when = (step) => {
      const ago = above === null ? "" : `${span((above - step.t) * 1000)} before`;
      above = step.t;
      return `<a class="dim when" data-stand="${Math.floor(step.t / 1000) + 1}" title="t=${(step.t / 1e6).toFixed(3)} ms of the board's: stand it here">${ago || `t=${(step.t / 1e6).toFixed(3)} ms`}</a>`;
    };
    const stepHtml = ({ step }) => {
      const address = step.byte ?? step.addr;
      const lit = address !== undefined ? ` data-lit="${address}"` : "";
      const v = byteOf(step);
      switch (step.what) {
        case "store":
          return `<li${lit}><span class="verb">stored</span> ${v === null ? "" : `<b>${byte(v)}</b> `}to ${place(step)} by ${by(step)} ${when(step)}</li>`;
        case "held": {
          const other = step.ram && step.written?.by?.pc !== undefined ? ` <a data-retrail="${address},${step.written.stamp * 1e6}" title="follow this value back instead">follow this one</a>` : "";
          const went = ` <a data-reach="${address},${step.t * 1000 - 1000}" title="everywhere this value went from here, and anything made of it">where else it went</a>`;
          return `<li${lit}><span class="verb">loaded</span> ${v === null ? "" : `<b>${byte(v)}</b> `}from ${place(step)} by ${by(step)} ${when(step)}${other}${went}</li>`;
        }
        case "read":
          return `<li${lit}><span class="verb">read</span> <b>${hex(step.value)}</b> off <b>${e(step.register)}</b>${step.record ? ` <a data-record="${step.record.n}" data-stand="${Math.floor(step.record.t / 1000) + 1}" title="stand the board at the trace record">trace record ${step.record.n}</a>` : ""} by ${by(step)} ${when(step)}</li>`;
        case "register":
          return `<li><span class="verb">held</span> in ${e(step.register)} as the window began</li>`;
        case "load": {
          // What chose the entry read: each value its address was worked
          // out from, a story of its own to follow.
          const leaves = step.address.map((leaf) => {
            const at = leaf.byte ?? leaf.addr;
            const follow = leaf.what === "held" && leaf.ram && leaf.written?.by?.pc !== undefined ? ` <a data-retrail="${at},${leaf.written.stamp * 1e6}" title="where this value came from">why</a>` : "";
            return `<span class="leaf"${at !== undefined ? ` data-lit="${at}"` : ""}>${e(leafText(leaf))}${follow}</span>`;
          });
          return `<li class="story-address"${lit}><span class="verb">chosen</span> by ${leaves.join(" and ")} <span class="dim">— what the address it was read at was worked out from</span></li>`;
        }
        case "merge":
          return `<li${lit}><span class="verb">made</span> of two values by ${by(step)} ${when(step)}<details><summary class="dim">the other</summary><ol>${step.other.map((o) => stepHtml({ step: o })).join("")}</ol></details></li>`;
        default:
          return "";
      }
    };
    // The steps, latest first, a run of copies folded into a line of its own.
    let rows = "";
    let hop = 0;
    for (const entry of folded(stepsOf(t))) {
      const first = entry.copies ? entry.copies[0] : entry;
      if (first.hop !== hop) {
        hop = first.hop;
        rows += `<li class="story-hop dim">which was written there earlier</li>`;
      }
      if (!entry.copies) {
        rows += stepHtml(entry);
        continue;
      }
      const stores = entry.copies.filter((c) => c.step.what === "store");
      const from = entry.copies.at(-1).step;
      const to = entry.copies[0].step;
      const names = [...new Set(entry.copies.map((c) => functionOf(c.step)))].join(", ");
      const summaryRow = `<span class="verb">copied</span> from ${place(from)} to ${place(to)} by ${e(names)}${stores.length > 1 ? ` <span class="dim">(${stores.length} copies)</span>` : ""} ${when(to)}`;
      above = from.t;
      rows += `<li class="story-copies"><details><summary>${summaryRow}</summary><ol>${entry.copies.map(stepHtml).join("")}</ol></details></li>`;
    }
    const said = summary(t);
    const ends = {
      more: () => `followed ${t.hops.length} writes back · <a data-trail-more title="go on, from the last write">keep going</a>`,
      stopped: () => `stopped${t.end.at !== undefined ? ` · <a data-trail-more title="go on from where it stopped">go on</a>` : ""}`,
    };
    const end = t.end && (ends[t.end.kind]?.() ?? null);
    const index = this.stories.indexOf(t);
    const nav = `<button type="button" data-story="${index - 1}" ${index > 0 ? "" : "disabled"} title="the story before">‹</button><button type="button" data-story="${index + 1}" ${index < this.stories.length - 1 ? "" : "disabled"} title="the story after">›</button>`;
    const value = stepsOf(t)[0]?.step;
    const head = `<div class="story-head"><span class="controls">${nav}</span> <b>where ${e(t.label ?? hex(t.address))}${value && byteOf(value) !== null ? ` = ${byte(byteOf(value))}` : ""} came from</b> <span class="dim">${hex(t.address)}</span> <span class="controls">${t.running ? `<button type="button" data-story-stop>stop</button>` : ""}${t.hops[0] ? `<button type="button" data-reach="${t.address},${t.hops[0].time}" title="follow this value forward from here: everywhere it, and anything made of it, went">where it went</button>` : ""}<button type="button" data-close="trail" title="close the story; ‹ brings it back">close</button></span></div>`;
    let html = `<div class="mem-story">${head}`;
    if (said) html += `<div class="story-summary">${e(said)}</div>`;
    if (rows) html += `<ol class="story-steps">${rows}</ol>`;
    if (t.running?.window) html += `<div class="dim">step ${t.hops.length + 1} back (at most ${HOPS} before it asks): following every value over the ${span(t.running.window)} before the write at ${(t.running.time / 1e9).toFixed(3)} ms</div><div data-progress="trail">${progressHtml(t.running.progress)}</div>`;
    else if (t.running) html += `<div class="dim">reading when it was last written…</div>`;
    if (t.error) html += `<div class="dim">${e(t.error)}</div>`;
    if (end) html += `<div class="story-end dim">${end}</div>`;
    return `${html}</div>`;
  }

  /// A task's name in its colour, as the owners' list shows it.
  ownerChip(name) {
    const owner = this.view.owners.find((o) => o.name === name);
    return `<span class="chip" style="--chip:${owner?.color ?? "var(--dim)"}">${this.e(name)}</span>`;
  }

  /// The story's way through memory, as arrows from where each value was
  /// to where it went next.
  storyArrows() {
    if (!this.trail) return [];
    const places = stepsOf(this.trail)
      .map(({ step }) => step.byte ?? step.addr)
      .filter((a) => a !== undefined);
    const arrows = [];
    for (let i = places.length - 1; i > 0; i--) if (places[i] !== places[i - 1]) arrows.push([places[i], places[i - 1]]);
    return arrows;
  }

  // ---- bytes in their own form -------------------------------------------------

  /// Sees a run of bytes in its own form: see `views.js`.
  async openView(key, address, size, name) {
    if (this.viewing?.key === key) {
      this.viewing = null;
      return this.draw(true);
    }
    this.scanned = (await this.view.read("board/scanned").catch(() => null))?.layers ?? [];
    const bytes = [];
    for (let a = address; a < address + Math.min(size, 4096); a++) bytes.push(this.view.byteAt(a));
    const layer = layerFor(this.scanned, address, size);
    this.viewing = { key, address, size, name, mode: firstView(bytes, layer), width: null, offset: 0 };
    this.draw(true);
  }

  /// The bytes being seen, as their view has them: a picture, hex or text.
  viewHtml() {
    const e = this.e;
    const v = this.viewing;
    const layer = layerFor(this.scanned, v.address, v.size);
    const modes = [
      ["picture", "picture"],
      ["hex", "hex"],
      ["text", "text"],
    ]
      .map(([mode, label]) => `<button type="button" data-view-mode="${mode}"${mode === v.mode ? ' class="on"' : ""}>${label}</button>`)
      .join("");
    const close = `<a data-close="viewing">close</a>`;
    let body;
    if (v.mode === "picture") {
      const shape = shapeOf(layer, v.address, v.size, v.width);
      const said = layer && !v.width ? `as the display scans it: ${shape.width}×${shape.height} ${shape.format}${shape.clut ? " through its colour table" : ""}` : `a byte a pixel, in grey`;
      const widths = [32, 64, 128, 160, 240, 256, 320].map((w) => `<option value="${w}"${(layer ? v.width === w : w === shape.width) ? " selected" : ""}>${w}</option>`).join("");
      body = `<div class="dim">${said} · <label>width <select data-view-width>${layer ? `<option value="">the display's</option>` : ""}${widths}</select></label> · click a pixel to ask where it came from</div><canvas class="mem-picture" width="${shape.width}" height="${shape.height}"></canvas><div class="dim mem-picture-readout">&nbsp;</div>`;
    } else {
      const page = 512;
      const from = v.offset;
      const to = Math.min(v.size, from + page);
      let rows = "";
      for (let row = from; row < to; row += 32) {
        let cells = "";
        for (let a = v.address + row; a < v.address + Math.min(row + 32, to); a++) {
          const b = this.view.byteAt(a);
          const shown = b === null ? "··" : v.mode === "hex" ? b.toString(16).padStart(2, "0") : b >= 0x20 && b < 0x7f ? e(String.fromCharCode(b)) : "·";
          cells += `<span class="b" data-heat="${a},${a + 1}" data-trail="${a}" title="${e(`${v.name}[${a - v.address}] = ${b === null ? "?" : `0x${b.toString(16)}`}: click to ask where it came from`)}">${shown}</span>`;
        }
        rows += `<div><span class="addr">${row.toString(16).padStart(5, "0")}</span> ${cells}</div>`;
      }
      const nav = v.size > page ? ` · <button type="button" data-view-page="${from - page}" ${from > 0 ? "" : "disabled"}>‹</button> ${from}–${to - 1} of ${v.size} <button type="button" data-view-page="${from + page}" ${to < v.size ? "" : "disabled"}>›</button>` : "";
      body = `<div class="dim">click a byte to ask where it came from${nav}</div><div class="mem-bytes mem-${v.mode}">${rows}</div>`;
    }
    return `<div class="mem-viewing"><div class="dim"><span class="controls">${modes}</span> ${e(v.name)} ${close}</div>${body}</div>`;
  }

  /// Paints the picture being seen, and makes each pixel a place to ask
  /// about; the story's byte, if it is one of them, marked.
  paintPicture() {
    const canvas = this.box.querySelector(".mem-picture");
    const v = this.viewing;
    if (!canvas || !v) return;
    const layer = layerFor(this.scanned, v.address, v.size);
    const shape = shapeOf(layer, v.address, v.size, v.width);
    const clut = clutBytes(shape.clut);
    const g = canvas.getContext("2d");
    const image = g.createImageData(shape.width, shape.height);
    for (let y = 0; y < shape.height; y++) {
      for (let x = 0; x < shape.width; x++) {
        const at = addressOf(shape, x, y);
        const inside = at >= v.address && at + shape.bytes <= v.address + v.size;
        const bytes = [];
        for (let i = 0; i < shape.bytes; i++) bytes.push(inside ? this.view.byteAt(at + i) : null);
        const c = colour(shape.format, bytes, clut) ?? [40, 40, 48];
        const p = 4 * (y * shape.width + x);
        image.data.set([c[0], c[1], c[2], 255], p);
      }
    }
    g.putImageData(image, 0, 0);
    // Where a value followed forward went, on the picture.
    g.fillStyle = "rgb(255 61 245 / 0.55)";
    for (const [start, length] of this.reachRuns()) {
      for (let a = start; a < start + length; a++) {
        const at = pixelOf(shape, a);
        if (at) g.fillRect(at.x, at.y, 1, 1);
      }
    }
    const marked = this.trail ? pixelOf(shape, this.trail.address) : null;
    if (marked) {
      g.strokeStyle = "#ff3df5";
      g.lineWidth = 1;
      g.strokeRect(marked.x - 2.5, marked.y - 2.5, 6, 6);
    }
    const scale = Math.max(1, Math.min(3, Math.floor(640 / shape.width)));
    canvas.style.width = `${shape.width * scale}px`;
    const pixel = (event) => {
      const box = canvas.getBoundingClientRect();
      const x = Math.floor(((event.clientX - box.left) / box.width) * shape.width);
      const y = Math.floor(((event.clientY - box.top) / box.height) * shape.height);
      const address = addressOf(shape, x, y);
      return x >= 0 && y >= 0 && x < shape.width && y < shape.height && address >= v.address && address < v.address + v.size ? { x, y, address } : null;
    };
    const readout = this.box.querySelector(".mem-picture-readout");
    canvas.onmousemove = (event) => {
      const at = pixel(event);
      if (!at || !readout) return;
      const b = this.view.byteAt(at.address);
      const [stamp, writer] = this.view.latestIn(at.address, at.address + 1);
      const by = stamp ? ` · written <span data-ago="${stamp}"></span> by ${this.writerHtml(this.view.inspected?.writers[writer])}` : "";
      readout.innerHTML = `(${at.x}, ${at.y}) = ${b === null ? "?" : `0x${b.toString(16).padStart(2, "0")}`} · 0x${at.address.toString(16)}${by}`;
      this.warm();
    };
    canvas.onclick = (event) => {
      const at = pixel(event);
      if (at) this.readTrail(at.address, `${v.name} (${at.x}, ${at.y})`);
    };
  }

  auditHtml() {
    const e = this.e;
    const head = `<div class="dim">who wrote what, across the whole board, over the last <span class="controls">${this.backsHtml()}</span> <span class="controls"><button type="button" data-audit>${this.audit ? "audit again" : "audit"}</button></span></div>`;
    const a = this.audit;
    if (!a) return `${head}<div class="dim">re-runs the board to here counting which code writes which word, and says any code that wrote on from one variable into the next</div>`;
    if (a.running) return `${head}<div class="dim">running the board again to here, counting every write</div><div data-progress="audit">${progressHtml(a.progress)}</div>`;
    if (a.error) return `${head}<div class="dim">${e(a.error)}</div>`;
    const r = a.answer;
    if (!r?.auditing) return `${head}<div class="dim">nothing audited: no board at the checkpoint it began from</div>`;
    const site = (n) => {
      const s = r.sites[n];
      return s?.via ? `${this.writerHtml(s.site)} <span class="dim">through ${e(s.via.replace(/ \+ 0x.*$/, ""))}</span>` : this.writerHtml(s);
    };
    const name = (o) => `${e(o.owner)}: ${e(o.name.split("::").slice(-2).join("::"))}`;
    const findings = r.findings.length
      ? r.findings.map((f) => `<div class="mem-finding">⚠ ${site(f.site)} wrote on from <b>${name(f.from)}</b> into <b>${name(f.into)}</b> at 0x${f.address.toString(16)}</div>`).join("")
      : `<div class="mem-ok">no code wrote on from one variable into the next</div>`;
    const owner = this.view.selected;
    const objects = r.objects
      .filter((o) => !owner || o.object.owner === owner)
      .map((o) => `<tr><td><b>${name(o.object)}</b> <span class="dim">${o.object.size} B</span></td><td>${o.sites.map(([n, words, writes]) => `${site(n)} <span class="dim">${words} word${words === 1 ? "" : "s"}, ${writes}×</span>`).join("<br>")}</td></tr>`)
      .join("");
    const unowned = r.unowned.map(([n, first, words, writes]) => `<tr><td>0x${first.toString(16)} <span class="dim">${words} words</span></td><td>${site(n)} <span class="dim">${writes}×</span></td></tr>`).join("");
    return `${head}${findings}${r.full ? `<div class="dim">too much to count all of: the window shortened would count it</div>` : ""}<div class="dim">${owner ? `${e(owner)}'s` : "every"} statics and stacks, and what wrote them</div><table class="mem-table">${objects}</table>${unowned ? `<div class="dim">memory no DWARF describes, and what wrote it</div><table class="mem-table">${unowned}</table>` : ""}`;
  }

  changesHtml() {
    const e = this.e;
    if (this.view.mark === null) return `<div class="dim">set the mark (above) at one instant; this then says every field of every task that changed since</div>`;
    if (!this.snapshot?.owners) return `<div class="dim">reading every task's statics at the mark…</div>`;
    const head = `<div class="dim">every field of every task that changed since the mark <span class="controls"><button type="button" data-compare>${this.compared ? "compare again" : "compare now"}</button></span></div>`;
    if (!this.compared) return head;
    if (this.compared.running) return `${head}<div class="dim">reading every task's statics…</div>`;
    if (!this.compared.out.length) return `${head}<div class="dim">nothing changed</div>`;
    const rows = this.compared.out
      .map(({ owner, name, found, ring }) => {
        const list = found
          .slice(0, 12)
          .map((c) => `<div class="chg"><span class="name">${e(c.path.slice(1).join(".") || "(all of it)")}</span> <span class="dim">${e(c.before)}</span> → <b>${e(c.after)}</b></div>`)
          .join("");
        const more = found.length > 12 ? `<div class="dim">…${found.length - 12} more</div>` : "";
        const log = ring?.added ? `<div class="dim">its log: ${ring.added} new entr${ring.added === 1 ? "y" : "ies"}, latest ${e(ring.last.at(-1)?.payload ?? "")}</div>` : "";
        return `<div class="mem-static"><div><b>${e(owner)}</b>: ${e(name.split("::").slice(-2).join("::"))}</div>${list}${more}${log}</div>`;
      })
      .join("");
    return `${head}${rows}`;
  }

  sourceHtml() {
    const e = this.e;
    if (!this.source) return "";
    const { file, line, whole } = this.source;
    const text = this.fileOf(file);
    const close = `<a data-close="source">close</a>`;
    const head = `<div class="mem-source-head"><b>${e(file.split("/").slice(-3).join("/"))}</b>:${line} <span class="dim" title="${e(file)}">${e(file)}</span> <a data-whole>${whole ? "around the line" : "the whole file"}</a> ${close}</div>`;
    if (text === undefined) return `<div class="mem-source">${head}<div class="dim">reading…</div></div>`;
    if (text === null) return `<div class="mem-source">${head}<div class="dim">the build did not keep this file</div></div>`;
    const lines = text.split("\n");
    const from = whole ? 0 : Math.max(0, line - 1 - CONTEXT);
    const to = whole ? lines.length : Math.min(lines.length, line + CONTEXT);
    const owner = this.view.selected;
    const held = owner ? this.writers.get(owner) : null;
    const rows = [];
    for (let n = from; n < to; n++) {
      const w = held?.lines.get(`${file}:${n + 1}`);
      const heat = w ? `<span class="heat" data-heat-stamp="${w.latest}" title="${w.words * 4} bytes of ${e(owner)}'s memory last written here"></span>` : `<span class="heat none"></span>`;
      rows.push(`<div class="sl${n + 1 === line ? " here" : ""}"${w ? ` data-runs="${e(JSON.stringify(w.runs))}"` : ""}><span class="ln">${n + 1}</span>${heat}<span class="code">${e(lines[n]) || " "}</span>${w ? ` <span class="dim wrote">${w.words * 4} B</span>` : ""}</div>`);
    }
    return `<div class="mem-source">${head}<div class="mem-source-body">${rows.join("")}</div><div class="dim">a line's colour: how lately it wrote what of ${owner ? e(owner) : "the task"}'s memory it holds; click one to outline that memory</div></div>`;
  }

  hovered(event) {
    const lit = event.target.closest("[data-lit]");
    const address = lit ? Number(lit.dataset.lit) : null;
    if (address !== this.lit) {
      this.lit = address;
      this.view.setHighlight(address === null ? (this.reach?.answer ? this.reachRuns() : null) : [[address, 1]]);
    }
    const hover = this.box.querySelector(".mem-hover");
    const byte = event.target.closest("[data-heat]");
    if (!hover) return;
    hover.textContent = byte?.title ?? " ";
  }

  // ---- clicks -------------------------------------------------------------------

  /// A click in the inspector: true when it was the inspector's.
  clicked(event) {
    const t = (selector) => event.target.closest(selector);
    const nodeOf = (key) => this.findNode(key);
    if (t("[data-mode]")) {
      this.mode = t("[data-mode]").dataset.mode;
      if (this.mode === "changes" && this.snapshot && !this.compared) this.compare();
      this.draw(true);
      this.refresh();
      return true;
    }
    if (t("[data-toggle]")) {
      const key = t("[data-toggle]").dataset.toggle;
      if (this.open.has(key)) this.open.delete(key);
      else this.open.add(key);
      this.draw(true);
      return true;
    }
    if (t("[data-back]")) {
      this.back = Number(t("[data-back]").dataset.back);
      if (this.history) this.readHistory(this.history);
      else this.draw(true);
      return true;
    }
    if (t("[data-history]")) {
      const target = nodeOf(t("[data-history]").dataset.history);
      if (target) this.readHistory(target);
      return true;
    }
    if (t("[data-why]")) {
      const target = nodeOf(t("[data-why]").dataset.why);
      if (target) this.readTrail(target.address, target.name);
      return true;
    }
    if (t("[data-trail]")) {
      const address = Number(t("[data-trail]").dataset.trail);
      this.readTrail(address, null);
      return true;
    }
    if (t("[data-retrail]")) {
      const [at, time] = t("[data-retrail]").dataset.retrail.split(",").map(Number);
      this.readTrail(at, null, { at, time });
      return true;
    }
    if (t("[data-reach]")) {
      const [address, from] = t("[data-reach]").dataset.reach.split(",").map(Number);
      this.readReach(address, null, Math.max(0, from));
      return true;
    }
    if (t("[data-reach-window]") && this.reach) {
      this.readReach(this.reach.address, this.reach.label, this.reach.from, Number(t("[data-reach-window]").dataset.reachWindow));
      return true;
    }
    if (t("[data-view]")) {
      const target = nodeOf(t("[data-view]").dataset.view);
      if (target) this.openView(target.key, target.address, target.size, target.name);
      return true;
    }
    if (t("[data-view-raw]")) {
      const [address, size] = t("[data-view-raw]").dataset.viewRaw.split(",").map(Number);
      this.openView(`raw ${address}`, address, size, t("[data-view-raw]").dataset.name);
      return true;
    }
    if (t("[data-view-mode]") && this.viewing) {
      this.viewing.mode = t("[data-view-mode]").dataset.viewMode;
      this.draw(true);
      return true;
    }
    if (t("[data-view-page]") && this.viewing) {
      this.viewing.offset = Math.max(0, Number(t("[data-view-page]").dataset.viewPage));
      this.draw(true);
      return true;
    }
    if (t("[data-story]")) {
      const story = this.stories[Number(t("[data-story]").dataset.story)];
      if (story) {
        this.trail = story;
        this.draw(true);
      }
      return true;
    }
    if (t("[data-story-stop]") && this.trail) {
      this.trail.stopped = true;
      this.draw(true);
      return true;
    }
    if (t("[data-trail-more]") && this.trail?.end?.at !== undefined) {
      const { at, time } = this.trail.end;
      this.readTrail(this.trail.address, this.trail.label, { at, time }, true);
      return true;
    }
    if (t("[data-goto]")) {
      this.view.goTo(Number(t("[data-goto]").dataset.goto));
      return true;
    }
    if (t("[data-close]")) {
      this[t("[data-close]").dataset.close] = null;
      this.draw(true);
      return true;
    }
    if (t("[data-audit]")) {
      this.runAudit();
      return true;
    }
    if (t("[data-compare]")) {
      this.compare();
      return true;
    }
    if (t("[data-src]")) {
      const a = t("[data-src]");
      this.source = { file: a.dataset.src, line: Number(a.dataset.line), whole: false };
      const owner = this.view.selected;
      if (owner) this.readWriters(owner);
      this.draw(true);
      return true;
    }
    if (t("[data-whole]")) {
      this.source.whole = !this.source.whole;
      this.draw(true);
      return true;
    }
    if (t("[data-runs]")) {
      const runs = JSON.parse(t("[data-runs]").dataset.runs);
      this.view.setHighlight(runs);
      return true;
    }
    return false;
  }

  /// The node a key names, decoded afresh: a static's, a frame's
  /// variable's, or a part of either. `{ key, owner, address, size, type }`.
  findNode(key) {
    const owner = this.view.selected;
    const [first, ...path] = key.split("/");
    let root = null;
    if (first === "stack") {
      const held = this.stacks.get(owner);
      const frame = held?.answer?.frames?.[Number(path.shift())];
      const v = frame?.variables[Number(path.shift())];
      if (!v || v.address === null) return null;
      root = decode(held.types, v.type, v.address, hexBytes(v.hex, v.address), v.name);
    } else {
      const typed = this.typedNow(owner);
      const st = typed?.statics.find((s) => s.address === Number(first));
      if (!st) return null;
      root = decode(typed.types, st.type, st.address, (a) => this.view.byteAt(a), st.name);
    }
    let node = root;
    const named = [root.name.split("::").pop()];
    for (const name of path) {
      // A wrapper's own field says nothing: `FRAME_SHOWN[40]`, not
      // `FRAME_SHOWN.value.value.__0[40]`.
      if (!WRAPPERS.test(node.typeName ?? "") && !(node.children?.length === 1 && /^__\d+$/.test(name))) named.push(name);
      node = node.children?.find((c) => c.name === name);
      if (!node) return null;
    }
    const name = named.join(".").replaceAll(".[", "[");
    return { key, owner, name, address: node.address, size: node.size, type: node.type };
  }
}

