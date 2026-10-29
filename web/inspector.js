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
import { follow } from "./trail.js";

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
      history.answer = await this.view.query({ watch: `range ${target.address} ${Math.max(1, target.size)}`, back: this.back }, "board/watched");
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
    const trail = more ? this.trail : { address, label, hops: [] };
    trail.from = from;
    trail.end = null;
    trail.error = null;
    this.trail = trail;
    const wanted = () => this.trail === trail;
    const update = () => wanted() && this.draw(true);
    trail.running = { at: from?.at ?? address, time: from?.time ?? null, window: 0 };
    update();
    try {
      await follow(trail, { read: (path) => this.view.read(path), query: (options, path) => this.view.query(options, path), update, wanted });
    } catch (why) {
      trail.error = String(why);
    }
    trail.running = null;
    update();
  }

  async runAudit() {
    this.audit = { back: this.back, running: true };
    this.draw(true);
    const audit = this.audit;
    try {
      audit.answer = await this.view.query({ watch: "audit", back: this.back }, "board/audit");
    } catch (why) {
      audit.error = String(why);
    }
    audit.running = false;
    if (this.audit === audit) this.draw(true);
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
    if (this.trail) body = this.trailHtml() + body;
    box.innerHTML = `${head}<div class="mem-views"><div class="mem-view">${body}</div>${this.sourceHtml()}</div>`;
    box.onmouseover = (event) => this.hovered(event);
    this.warm();
    const near = box.querySelector(".near");
    if (near && this.focusShown !== this.view.focus) {
      this.focusShown = this.view.focus;
      near.scrollIntoView({ block: "nearest" });
    }
    this.view.setArrows(this.arrows ?? []);
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
      ? `<span class="acts"><a data-history="${e(key)}" title="every write to it over the window, from a re-execution watching it">history</a> <a data-why="${e(key)}" title="where its value came from: each copy and each write back to where it came in, from re-executions following every value">why</a></span>`
      : "";
    const near = this.view.focus !== null && depth === 0 && this.view.focus >= node.address && this.view.focus < node.address + node.size;
    let html = `<div class="mem-node${near ? " near" : ""}${node.unknown ? " unknown" : ""}" style="padding-left:${depth * 1.2}em" data-key="${e(key)}">${toggle}${heat}<span class="name">${e(depth ? node.name : node.name.split("::").slice(-2).join("::"))}</span><span class="ty">: ${e(node.typeName)}</span> = <span class="val">${e(node.value)}</span>${by}${act}</div>`;
    if (this.history?.key === key) html += this.historyHtml(node);
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
    if (h.running) return `${head}<div class="dim">re-running the board to here, watching it…</div></div>`;
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
    const at = (address, where) => `<a data-goto="${address}" title="show ${hex(address)} in the pictures">${e(where ?? hex(address))}</a>`;
    const when = (ns) => `<a class="dim" data-stand="${Math.floor(ns / 1000) + 1}" title="stand the board here">t=${(ns / 1e6).toFixed(3)} ms</a>`;
    const by = (s) => (s.lines?.length ? this.writerHtml(s) : e(s.code ?? "?"));
    // A word's value as the byte followed, where the step is about one of its bytes.
    const lane = (s) => s.size > 1 && s.byte - s.addr >= 0 && s.byte - s.addr < s.size;
    const value = (s) => (lane(s) ? `${hex((s.value >>> (8 * (s.byte - s.addr))) & 0xff)} <span class="dim">(of ${hex(s.value)})</span>` : hex(s.value));
    const place = (s) => (lane(s) ? s.byte : s.addr);
    const step = (s, last, nested) => {
      switch (s.what) {
        case "store":
          return `<li>${value(s)} stored to ${at(place(s), s.where)} by ${by(s)} ${when(s.t)}${last ? ` — <b>made by that code: a constant</b>` : ""}</li>`;
        case "read":
          return `<li><b>came in off ${e(s.register)}</b>: ${hex(s.value)}${s.record ? `, trace record ${s.record.n}` : ""}, read by ${by(s)} ${when(s.t)}</li>`;
        case "held": {
          const again = nested && s.ram && s.written?.by?.pc !== undefined ? ` <a data-retrail="${s.byte ?? s.addr},${s.written.stamp * 1e6}" title="follow this value back instead">follow this one</a>` : "";
          return `<li>${value(s)} loaded from ${at(place(s), s.where)} by ${by(s)} ${when(s.t)}${again}</li>`;
        }
        case "register":
          return `<li>held in ${e(s.register)} as the window began</li>`;
        case "merge":
          return `<li>made of two values by ${by(s)} ${when(s.t)}<details><summary class="dim">the other</summary><ol>${steps(s.other, true)}</ol></details></li>`;
        default:
          return "";
      }
    };
    const steps = (list, nested = false) => list.map((s, i) => step(s, i === list.length - 1, nested)).join("");
    const window = (ps) => (ps >= 1e12 ? `${ps / 1e12} s` : `${ps / 1e9} ms`);
    let html = `<div class="mem-trail"><div class="dim">where the value at ${at(t.address, t.label)} came from, write by write <a data-close="trail">close</a></div>`;
    t.hops.forEach((hop, i) => {
      const lead = i === 0 ? "as it stands" : `which was written there`;
      html += `<div class="trail-hop dim">${lead}: ${at(hop.at, hop.what)} — the board run again over the ${window(hop.window)} to ${when(hop.time / 1000)}</div>`;
      html += hop.steps.length ? `<ol class="mem-why">${steps(hop.steps)}</ol>` : "";
    });
    if (t.running?.window) html += `<div class="dim">running the board again over the ${window(t.running.window)} before ${(t.running.time / 1e9).toFixed(3)} ms, following everything…</div>`;
    else if (t.running) html += `<div class="dim">reading when it was last written…</div>`;
    if (t.error) html += `<div class="dim">${e(t.error)}</div>`;
    const end = t.end;
    const last = end?.step;
    const ends = {
      bus: () => `it came in from outside, off ${e(last.register)}`,
      rom: () => `it came from ${e(last.where)}`,
      constant: () => `the code that stored it made it`,
      unwritten: () => `never written since the board started: what the RAM held at reset`,
      writer: () => `written by ${e(end.by?.code ?? last?.written?.by?.code ?? "something not an instruction")}`,
      register: () => `it sat in ${e(last?.register ?? "a register")} longer than the widest window: followed no further`,
      unfollowed: () => `nothing followed in the window wrote it: the probe, or DMA`,
      more: () => `followed ${t.hops.length} writes back <a data-trail-more title="go on, from the last write">keep going</a>`,
    };
    if (end) html += `<div class="trail-end">${(ends[end.kind] ?? (() => e(end.kind)))()}</div>`;
    return `${html}</div>`;
  }

  auditHtml() {
    const e = this.e;
    const head = `<div class="dim">who wrote what, across the whole board, over the last <span class="controls">${this.backsHtml()}</span> <span class="controls"><button type="button" data-audit>${this.audit ? "audit again" : "audit"}</button></span></div>`;
    const a = this.audit;
    if (!a) return `${head}<div class="dim">re-runs the board to here counting which code writes which word, and says any code that wrote on from one variable into the next</div>`;
    if (a.running) return `${head}<div class="dim">re-running the board to here, counting every write…</div>`;
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
    if (t("[data-trail-more]") && this.trail?.end?.kind === "more") {
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
    for (const name of path) {
      node = node.children?.find((c) => c.name === name);
      if (!node) return null;
    }
    const name = [root.name.split("::").pop(), ...path].join(".").replaceAll(".[", "[");
    return { key, owner, name, address: node.address, size: node.size, type: node.type };
  }
}

