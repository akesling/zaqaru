// The page: a store browser with a time axis. The container is a StructFS
// store; every panel is a read of one of its paths at the chosen instant,
// and the panels themselves come from the store's own `meta` lens rather
// than from a list kept here. The timeline is the container's traffic with
// the host — its reads and writes under `/iso` — beside its syscalls, and
// the row you click chooses what the panels look at: a path in a syscall's
// arguments opens the file, a descriptor opens what it names, bytes on a
// connection open the socket. Everything the page knows comes from the
// worker.
//
// Three ways in. With a tape, the run is fixed and every instant is a seek.
// Live, the container runs against this page's clock and entropy; "play"
// advances the frontier, the slider views anything behind it, and a request
// typed into the edge box goes to a listener inside the container. With a
// snapshot, the live run starts from a container somebody already booted.
// Live, the page opens on the edge box alone — nothing has happened yet —
// and when the first answer arrives it stands the machine on the instant
// the request came in and shows everything.

import { request as requestUsb } from "./usb.js";
import * as bench from "./bench.js";
import { MemoryView } from "./memory.js";
import { ScreenView } from "./screen.js";
import { FIELDS, Options, emptySpec, isEmpty, matches } from "./filters.js";

const worker = new Worker("./worker.js", { type: "module" });
const $ = (id) => document.getElementById(id);

const LIVE_TICK = 4000000; // instructions per advance while playing a tape
const FOLLOW_MS = 100; // live: how often the panels follow the running frontier

let live = false;
let origin = 0;
let total = 0;
let frontier = 0;
let finished = null;
let timeline = []; // { at, pid, tid, name } per syscall
let trace = []; // the strace line for each
let exchanges = []; // { op, path, syscall, bytes, text, error } per host exchange
// The board's OS-level records, as the worker reports them from the live
// run: each Hubris system call, fault, reset and halt, and which task ran
// — `{ n, t, at, ch, ... }`, `t` in the board's nanoseconds, `at` the
// container's instant the board was moved on at — each call and fault
// with the task that made it.
let hubris = [];
let hubrisTask = null;
function takeHubris(records) {
  for (const record of records ?? []) {
    if (record.ch === "TASK") hubrisTask = record.task;
    else hubris.push({ ...record, task: hubrisTask });
  }
}
let events = []; // the two merged, as the timeline shows them
let spans = []; // { sent, answered } per request answered through the edge
let playing = null;
let checkpointing = null; // live: what checkpoints are costing, from the worker
let lastState = 0; // when the last state was drawn, by the page's clock
let seekCost = 0; // ms the last seek took the worker, restoring aside
let current = 0;
let busy = false;
let queued = null;
let nextRequest = 0;
let responses = 0;
let disassembled = 0;
let meta = null; // the store's meta lens
let interface_ = {}; // the manifest's description of each path
let context = { pid: null, tid: null, path: "" }; // what the panels look at
let values = {}; // the last state's values, by concrete path
let pinned = null; // { at, values } when pinned
let pending = null; // { at, kind, ... }: what to focus once the state at `at` arrives
let nextRead = 0;
const reads = new Map(); // id -> { resolve, reject }

const FILES = "processes/{pid}/files/{path}";
const DESCRIPTORS = "processes/{pid}/descriptors";

function clamp(at) {
  return Math.max(origin, Math.min(live ? frontier : total, Math.round(at)));
}

function seek(at) {
  at = clamp(at);
  boardTime = null;
  if (busy) {
    queued = { seek: at };
    return;
  }
  busy = true;
  worker.postMessage({ type: "seek", at, context });
}

/// Live: the panels at the frontier, wherever it has got to by the time the
/// worker reads them — never an instant behind it, which would be a
/// restore and a re-execution for a view that is stale on arrival.
function follow() {
  boardTime = null;
  if (busy) {
    queued = { follow: true };
    return;
  }
  busy = true;
  worker.postMessage({ type: "seek", at: null, context });
}

/// The board as it was at `time` picoseconds of its own time — and every
/// panel with it. The container's count of instructions, which the slider
/// moves along, doesn't name such an instant: an idle container retires
/// none while the board runs on.
let boardTime = null;
// The board's time where the live run has got to: an instant past it is
// not in the recording to stand at.
let liveBoardTime = Infinity;
function seekBoard(time) {
  stop();
  boardTime = Math.max(0, Math.min(Math.round(time), liveBoardTime));
  if (busy) {
    queued = { board: boardTime };
    return;
  }
  busy = true;
  worker.postMessage({ type: "seek", at: null, context, board: { time: boardTime, follow: followValues } });
}

/// Whether a stand in the board's time follows where its values go on the
/// way there: what the values section and its `why`s are made of.
let followValues = false;

/// Back from an instant of the board's to the live run.
function backToLive() {
  boardTime = null;
  if (!playing && finished === null) $("play").click();
  else follow();
}

function advance(by) {
  if (busy) {
    queued = { advance: by };
    return;
  }
  busy = true;
  worker.postMessage({ type: "advance", by });
}

function drain() {
  if (queued === null) return false;
  const next = queued;
  queued = null;
  if (next.seek !== undefined) seek(next.seek);
  else if (next.board !== undefined) seekBoard(next.board);
  else if (next.follow) follow();
  else advance(next.advance);
  return true;
}

/// One path read at the board's instant in view (the live run's latest,
/// or the one stood at), from a container re-executed there with
/// `options` on the way: `{ watch, back }` to watch writes from `back`
/// picoseconds before. What `memory.js` asks a variable's history and an
/// audit with; the panels stay where they are.
function query(options, path) {
  return new Promise((resolve, reject) => {
    const id = nextRead++;
    reads.set(id, { resolve, reject });
    const time = boardTime ?? (Number.isFinite(liveBoardTime) ? liveBoardTime : null);
    if (time === null) return reject("no board time to stand at yet");
    worker.postMessage({ type: "query", id, board: { time, follow: false, ...options }, path });
  });
}

/// One path of the store, read at the instant in view.
function read(path) {
  return new Promise((resolve, reject) => {
    const id = nextRead++;
    reads.set(id, { resolve, reject });
    worker.postMessage({ type: "read", id, path });
  });
}

const mb = (n) => (n / 1048576).toFixed(1) + " MB";

function status(extra = "") {
  if (live) {
    const parts = [
      origin ? "live from a snapshot" : "live",
      `${(frontier - origin).toLocaleString()} instructions`,
      `${timeline.length} syscalls`,
      `${exchanges.length} exchanges`,
      checkpointing ? `checkpoints ${checkpointShare().toFixed(0)}% of run time` : "",
      extra,
      finished !== null ? `exited ${finished}` : "",
    ].filter(Boolean);
    $("status").textContent = parts.join(" · ");
    $("ckstat").textContent = checkpointCost();
  }
}

const seconds = (ms) => (ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(1)} s`);

function checkpointShare() {
  const c = checkpointing;
  return c && c.ran + c.spent ? (100 * c.spent) / (c.ran + c.spent) : 0;
}

/// What live checkpoints cost, in run time, and what they buy: how far a
/// seek may have to re-execute.
function checkpointCost() {
  const c = checkpointing;
  if (!c) return "";
  const share = checkpointShare();
  const replay = c.rate ? ` (~${seconds(c.widest / c.rate)})` : "";
  return `${c.count} taken · ${seconds(c.lastCost)} the last · ${share.toFixed(0)}% of run time · a seek replays ≤ ${(c.widest / 1e6).toFixed(1)}M instructions${replay}`;
}

/// Tells the worker how to space live checkpoints, from the controls.
function sendCheckpointing() {
  worker.postMessage({ type: "checkpointing", every: Number($("every").value), budget: Number($("budget").value) / 100 });
}

// ---- names and colours ---------------------------------------------------------
//
// A process is named by what it was started from — nginx, gunicorn — and
// keeps one colour for the run, so the timeline, the lane strip under the
// slider and the process cards all say the same thing.

const PALETTE = ["#58a6ff", "#3fb950", "#d29922", "#f778ba", "#a371f7", "#f0883e", "#79c0ff", "#56d364", "#e3b341", "#ff7b72"];
const colors = new Map(); // pid -> colour
const names = new Map(); // pid -> name

function colorOf(pid) {
  if (!colors.has(pid)) colors.set(pid, PALETTE[colors.size % PALETTE.length]);
  return colors.get(pid);
}

function nameOf(pid) {
  return names.get(pid) ?? `pid ${pid}`;
}

function learnNames(processes) {
  for (const p of processes ?? []) {
    const base = p.comm || (p.exe ?? "").split("/").filter(Boolean).pop();
    names.set(p.pid, base || `pid ${p.pid}`);
    colorOf(p.pid);
  }
}

/// The strip under the slider: which process ran when, from the pid on
/// each syscall's stamp, and a band for each request from sent to answered.
function drawLanes() {
  const canvas = $("lanes");
  const width = canvas.clientWidth || 1;
  const height = canvas.clientHeight || 12;
  const scale = window.devicePixelRatio || 1;
  if (canvas.width !== Math.round(width * scale) || canvas.height !== Math.round(height * scale)) {
    canvas.width = Math.round(width * scale);
    canvas.height = Math.round(height * scale);
  }
  const g = canvas.getContext("2d");
  g.setTransform(scale, 0, 0, scale, 0, 0);
  g.clearRect(0, 0, width, height);
  const end = live ? frontier : total;
  const span = Math.max(1, end - origin);
  const x = (at) => ((at - origin) / span) * width;
  g.fillStyle = "#8883";
  g.fillRect(0, 0, width, 8);
  for (let i = 0; i < timeline.length; i++) {
    const from = x(timeline[i].at);
    const to = i + 1 < timeline.length ? x(timeline[i + 1].at) : x(end);
    g.fillStyle = colorOf(timeline[i].pid);
    g.fillRect(from, 0, Math.max(1, to - from), 8);
  }
  g.fillStyle = "#58a6ff";
  for (const s of spans) g.fillRect(x(s.sent), 9, Math.max(2, x(s.answered) - x(s.sent)), 3);
  // The board's Hubris tasks: each system call at the instant the board was
  // moved on at, in its kind's colour; faults, resets and halts taller. A
  // pixel column at a time, the latest in it shown and a fault over
  // anything: a board playing a game makes hundreds of calls a second, and
  // a rectangle each, every redraw, is what kept the page too busy to draw
  // the game.
  if (hubris.length) {
    const columns = new Array(Math.ceil(width) + 1).fill(null);
    for (const record of hubris) {
      const column = Math.max(0, Math.min(columns.length - 1, Math.floor(x(record.at))));
      if (record.ch !== "SYS") columns[column] = KIND_COLORS.fault;
      else if (columns[column] !== KIND_COLORS.fault) columns[column] = record;
    }
    columns.forEach((what, column) => {
      if (what === null) return;
      if (what === KIND_COLORS.fault) {
        g.fillStyle = what;
        g.fillRect(column, 13, 2, 7);
      } else {
        g.fillStyle = KIND_COLORS[bench.syscallLine(what).kind] ?? "#888";
        g.fillRect(column, 14, 1.5, 5);
      }
    });
  }
}

worker.onmessage = (event) => {
  const message = event.data;
  if (message.type === "error") {
    $("status").textContent = message.message;
    busy = false;
    stop();
    return;
  }
  if (message.type === "loaded") {
    live = message.live;
    origin = message.origin;
    total = message.total;
    frontier = message.frontier;
    finished = message.finished;
    timeline = message.timeline;
    trace = message.trace;
    exchanges = message.exchanges;
    hubris = [];
    hubrisTask = null;
    meta = message.meta;
    interface_ = message.interface;
    spans = [];
    $("slider").min = origin;
    $("slider").max = live ? frontier : total;
    $("controls").classList.add("hidden");
    buildPanels();
    if (live) {
      $("port").value = message.published[0] ?? 8080;
      const where = message.listening.length ? `listening on ${message.listening.join(", ")}` : `publishing ${message.published.join(", ") || "no ports"}`;
      status(`${where} — send the request · loaded in ${(message.loading / 1000).toFixed(1)} s${message.inflated ? ` (${(message.inflated / 1000).toFixed(1)} s inflating)` : ""}`);
      // Nothing has happened yet: the edge box alone, until something has.
      if (message.published.length) document.body.classList.add("opening");
      // A terminal: open its session now, and run, so that whatever
      // answers on the port greets it.
      if (terminalPort) {
        document.body.classList.add("bench");
        openTerminal();
        if (!playing && finished === null) $("play").click();
      }
    } else {
      $("status").textContent = `${total.toLocaleString()} instructions · ${timeline.length} syscalls · ${exchanges.length} exchanges · ${message.checkpoints.length} checkpoints holding ${mb(message.held)} · ${message.bytecode ? "bytecode" : "interpreter"} · loaded in ${(message.loading / 1000).toFixed(1)} s`;
    }
    rebuildEvents();
    renderEvents(true);
    drawLanes();
    seek(origin);
    return;
  }
  if (message.type === "progress") {
    // A reply to `advance`, or the running frontier reporting on its own,
    // which the page answers by drawing the frontier now and then — and
    // the worker does not wait for.
    if (message.reply) busy = false;
    if (message.checkpointing) checkpointing = { ...message.checkpointing, count: message.checkpoints };
    frontier = message.frontier;
    finished = message.finished;
    timeline.push(...message.timeline);
    trace.push(...message.trace);
    exchanges.push(...message.exchanges);
    takeHubris(message.hubris);
    $("slider").max = frontier;
    status(`${message.listening.length ? `listening on ${message.listening.join(", ")}` : ""}${message.idle && finished === null ? " · idle" : ""}`);
    extendEvents();
    renderEvents(true);
    drawCustomFilter();
    drawLanes();
    // Whether the panels were following the frontier, before a finish stops the play.
    const following = playing !== null;
    if (finished !== null) stop();
    if (message.reply) {
      if (!drain()) seek(frontier);
    } else if (following && (message.stopped || finished !== null)) follow();
    // At most a twentieth of the worker's time reading every panel, which
    // a busy board needs the rest of.
    else if (following && !busy && performance.now() - lastState >= Math.max(FOLLOW_MS, 20 * seekCost)) follow();
    return;
  }
  // The USB devices the worker holds for the container: the page's grants.
  if (message.type === "usb") {
    granted = message.devices;
    const panel = panels.get("usb");
    if (panel && values.usb?.value) renderers.usb(values.usb.value, panel);
    return;
  }
  if (message.type === "display") {
    screenView?.pushed(message.value);
    return;
  }
  if (message.type === "state") {
    busy = false;
    lastState = performance.now();
    seekCost = message.took ?? 0;
    render(message);
    if (!drain() && playing && !live) seek(current + playing);
    return;
  }
  if (message.type === "stream") {
    if (message.id === examplesId) {
      if (message.data) examplesText += message.data;
      if (message.ended || /^on the board: .*\n/m.test(examplesText)) {
        examplesId = null;
        examplesListed(examplesText);
      }
      return;
    }
    if (message.id !== terminalId) return;
    if (message.data) terminalWrite(message.data);
    if (message.ended) terminalWrite(`\n[the session ended${message.error ? `: ${message.error}` : ""}]\n`);
    return;
  }
  if (message.type === "value") {
    const waiting = reads.get(message.id);
    reads.delete(message.id);
    if (!waiting) return;
    if (message.error !== undefined) waiting.reject(message.error);
    else waiting.resolve(message.value);
    return;
  }
  if (message.type === "response") {
    const box = $(`exchange-${message.id}`);
    if (!box) return;
    responses++;
    if (message.error) box.querySelector(".meta").textContent += ` — ${message.error}`;
    else answered(box, message);
  }
};

/// The instant a request sent at `sent` came into the container: the
/// accept that took the connection, else the first read of its bytes.
function arrivalOf(sent) {
  const accepted = timeline.find((t) => t.at >= sent && (t.name === "accept4" || t.name === "accept"));
  if (accepted) return accepted.at;
  for (const e of exchanges) {
    const at = timeline[e.syscall]?.at ?? frontier;
    if (at >= sent && e.op === "read" && /^iso\/net\/conn\/\d+\/rx/.test(e.path) && e.bytes > 0) return at;
  }
  return sent;
}

/// A response has come back through the edge: mark the request's span,
/// offer its two instants as links, and — the first time, from the opening
/// state — stand the machine on the instant the request arrived.
function answered(box, message) {
  spans.push({ sent: message.sent, answered: message.answered });
  drawLanes();
  const meta_ = box.querySelector(".meta");
  meta_.textContent = `#${message.id} sent at ${message.sent.toLocaleString()}, answered at ${message.answered.toLocaleString()} — `;
  const arrival = document.createElement("a");
  arrival.textContent = "seek to its arrival";
  arrival.onclick = () => go(arrivalOf(message.sent), null);
  const answer = document.createElement("a");
  answer.textContent = "to the answer";
  answer.onclick = () => go(message.answered, null);
  meta_.append(arrival, " · ", answer);
  const body = document.createElement("pre");
  body.textContent = message.response;
  box.appendChild(body);
  if (document.body.classList.contains("opening")) {
    leaveOpening();
    go(arrivalOf(message.sent), null);
  }
}

function leaveOpening() {
  if (!document.body.classList.contains("opening")) return;
  document.body.classList.remove("opening");
  drawLanes();
}

// ---- the timeline ------------------------------------------------------------
//
// Two kinds of event on one axis of retired instructions: the syscalls the
// kernel stamped, and the exchanges with the host — placed at the syscall
// each was made in. A run is a million events long before it is
// interesting, so the list holds rows only for a window around the present
// and rebuilds the window when the present leaves it.

const WINDOW = 150;
let rendered = { from: -1, to: -1, filter: "" };

function describeExchange(e) {
  const arrow = e.op === "read" ? "→" : "←";
  let what;
  if (e.error) what = `✗ ${e.error}`;
  else if (e.op === "read" && e.text === null && e.bytes === 0) what = "nothing there";
  else if (e.text !== null && e.text.length === e.bytes) what = JSON.stringify(e.text);
  else if (e.text !== null) what = `${e.bytes} bytes ${JSON.stringify(e.text)}…`;
  else what = `${e.bytes} bytes`;
  return `<span class="op">${e.op.padEnd(5)}</span> ${escape("/" + e.path)} ${arrow} ${escape(what)}`;
}

/// Whether an exchange is a poll that found nothing: the kernel, idle,
/// asking the host for network events or a shutdown it has not been given.
function emptyPoll(e) {
  return e.op === "read" && e.text === null && e.bytes === 0 && !e.error;
}

// The events list is kept as the run grows rather than rebuilt: each report
// adds the rows it brings, in order. Two things make that more than an
// append. An exchange is placed at the instant of the syscall it was made
// during, which is known only once that syscall returns; until then it is
// drawn at the frontier, and taken back and placed again at the next report.
// And repeats of one exchange within one syscall fold into one row, so the
// folding goes on from where the last report left it. Should a row ever
// arrive before the last one kept, the list is built again from the start.
let built = null;

// The custom filter (see `filters.js`): what is chosen, kept for this
// viewer, and the dropdowns' options, counted as the rows are built.
let customSpec = emptySpec();
try {
  customSpec = { ...customSpec, ...JSON.parse(localStorage.getItem("timeline-filter") ?? "{}") };
} catch {
  // No storage here: start from nothing chosen.
}
let customOptions = new Options();

/// What the list is built for: the filter, and with a custom one, what it
/// chooses.
function filterKey() {
  const filter = $("filter").value;
  return filter === "custom" ? `custom ${JSON.stringify(customSpec)}` : filter;
}

/// A row's fields, for the custom filter.
function fieldsOf(kind, index) {
  if (kind === "syscall") {
    const t = timeline[index];
    const line = (trace[index] ?? t.name ?? "").replace(/^\[\d+\] /, "");
    return { process: nameOf(t.pid), syscall: line.match(/^(\w+)\(/)?.[1] ?? t.name, text: line };
  }
  if (kind === "exchange") {
    const e = exchanges[index];
    const pid = timeline[e.syscall]?.pid;
    return { process: pid === undefined ? null : nameOf(pid), path: e.path, text: `${e.op} /${e.path} ${e.error ?? e.text ?? ""}` };
  }
  const record = hubris[index];
  const { who, text } = hubrisLine(record);
  const call = record.ch === "SYS" ? bench.syscallLine(record).name : record.ch === "FAULT" ? "fault" : "board";
  return { task: who, call, text };
}

function rebuildEvents() {
  built = { filter: filterKey(), timeline: 0, exchanges: 0, hubris: 0, stamp: -1, groups: new Map() };
  events = [];
  if ($("filter").value === "custom") customOptions = new Options();
  extendEvents();
}

function extendEvents() {
  const filter = $("filter").value;
  if (!built || built.filter !== filterKey()) return rebuildEvents();
  const raw = filter === "raw";
  // Custom: every kind, folded as "all" is, each row counted for the
  // dropdowns and shown if it passes.
  const custom = filter === "custom";
  const passes = (kind, index) => {
    if (!custom) return true;
    const fields = fieldsOf(kind, index);
    customOptions.note(fields);
    return matches(customSpec, fields);
  };
  // What was drawn at the frontier for want of its syscall's instant.
  while (events.length && events.at(-1).provisional) events.pop();
  if (events.some((e) => e.provisional)) return rebuildEvents();
  const fresh = [];
  if (filter === "all" || filter === "raw" || filter === "hubris" || custom) {
    for (let i = built.hubris; i < hubris.length; i++) if (passes("hubris", i)) fresh.push({ at: hubris[i].at, kind: "hubris", index: i, order: 2, count: 1 });
  }
  built.hubris = hubris.length;
  if (filter !== "hubris" && filter !== "exchange") {
    for (let i = built.timeline; i < timeline.length; i++) if (passes("syscall", i)) fresh.push({ at: timeline[i].at, kind: "syscall", index: i, order: 1, count: 1 });
  }
  built.timeline = timeline.length;
  if (filter !== "hubris" && filter !== "syscall") {
    // Folded: repeats of one path at one stamp — an idle container reads
    // the clock at every wake — become one row saying how many; polls that
    // found nothing are left out. "Everything" shows each as it was.
    const pending = [];
    let i = built.exchanges;
    for (; i < exchanges.length; i++) {
      const e = exchanges[i];
      const settled = e.syscall < timeline.length;
      if (!settled) break;
      if (!raw && emptyPoll(e)) continue;
      if (!passes("exchange", i)) continue;
      if (e.syscall !== built.stamp) {
        built.stamp = e.syscall;
        built.groups = new Map();
      }
      const key = `${e.op}|${e.path}`;
      const folded = !raw && !e.error ? built.groups.get(key) : undefined;
      if (folded) {
        folded.count++;
        continue;
      }
      const row = { at: timeline[e.syscall].at, kind: "exchange", index: i, order: 0, count: 1 };
      fresh.push(row);
      if (!raw && !e.error) built.groups.set(key, row);
    }
    built.exchanges = i;
    // The rest wait on their syscall: drawn at the frontier for now.
    for (; i < exchanges.length; i++) {
      if (!raw && emptyPoll(exchanges[i])) continue;
      // Counted once it is placed, not now as well.
      if (custom && !matches(customSpec, fieldsOf("exchange", i))) continue;
      pending.push({ at: frontier, kind: "exchange", index: i, order: 0, count: 1, provisional: true });
    }
    fresh.sort(byInstant);
    if (fresh.length && events.length && byInstant(fresh[0], events.at(-1)) < 0) {
      events.push(...fresh, ...pending);
      events.sort(byInstant);
      return;
    }
    events.push(...fresh, ...pending);
    return;
  }
  fresh.sort(byInstant);
  if (fresh.length && events.length && byInstant(fresh[0], events.at(-1)) < 0) return rebuildEvents();
  events.push(...fresh);
}

function byInstant(a, b) {
  return a.at - b.at || a.order - b.order || a.index - b.index;
}

/// The index of the last event at or before `current`.
function position() {
  let low = 0;
  let high = events.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if (events[middle].at <= current) low = middle + 1;
    else high = middle;
  }
  return low - 1;
}

// Which syscalls take a descriptor first, and which hand one back, so the
// numbers in a trace line can be links to what they name.
const FD_TAKING = new Set(["read", "write", "close", "fstat", "lseek", "ioctl", "pread64", "pwrite64", "readv", "writev", "sendto", "recvfrom", "sendmsg", "recvmsg", "shutdown", "bind", "listen", "accept", "accept4", "connect", "getsockname", "getpeername", "setsockopt", "getsockopt", "fcntl", "flock", "fsync", "fdatasync", "ftruncate", "getdents64", "fchdir", "fchmod", "fchown", "epoll_ctl", "epoll_wait", "dup", "dup2", "dup3", "sendfile", "mmap"]);
const FD_RETURNING = new Set(["open", "openat", "socket", "accept", "accept4", "dup", "dup2", "dup3", "epoll_create1", "eventfd2", "memfd_create", "socketpair"]);

function numberOf(token) {
  return token.startsWith("0x") ? parseInt(token.slice(2), 16) : Number(token);
}

function fdLink(token) {
  const fd = numberOf(token);
  return fd >= 0 && fd < 65536 ? `<a class="link fd" data-fd="${fd}">${token}</a>` : token;
}

/// A trace line with its paths and descriptors as links.
function linkify(line) {
  const m = line.match(/^(\w+)\((.*)\) = (.*)$/s);
  if (!m) return escape(line);
  const [, name, args, ret] = m;
  let html = escape(args).replace(/&quot;(\/[^&]*?)&quot;/g, (_, path) => `&quot;<a class="link file" data-path="${path}">${path}</a>&quot;`);
  if (FD_TAKING.has(name)) {
    const which = name === "mmap" ? 4 : 0;
    const parts = html.split(", ");
    if (parts[which] !== undefined && /^(0x[0-9a-f]+|\d+)$/.test(parts[which])) parts[which] = fdLink(parts[which]);
    if (name === "epoll_ctl" && parts[2] !== undefined && /^(0x[0-9a-f]+|\d+)$/.test(parts[2])) parts[2] = fdLink(parts[2]);
    html = parts.join(", ");
  }
  const retHtml = FD_RETURNING.has(name) && /^\d+$/.test(ret) ? fdLink(ret) : escape(ret);
  return `${name}(${html}) = ${retHtml}`;
}

/// What a row is about, for the panels to open on: the first path in a
/// syscall's arguments, else the descriptor it took; a connection's edge
/// for bytes on it; the console for what was written there.
function focusOf(event) {
  if (event.kind === "syscall") {
    const line = trace[event.index] ?? "";
    const path = line.match(/"(\/[^"]*)"/);
    if (path) return { kind: "file", path: path[1] };
    const m = line.match(/^\[\d+\] (\w+)\((0x[0-9a-f]+|\d+)/);
    if (m && FD_TAKING.has(m[1]) && m[1] !== "mmap") {
      const fd = numberOf(m[2]);
      if (fd < 65536) return { kind: "fd", fd };
    }
    return null;
  }
  const e = exchanges[event.index];
  const conn = e.path.match(/^iso\/net\/conn\/(\d+)\//);
  if (conn) return { kind: "edge", edge: Number(conn[1]) };
  if (e.path.startsWith("iso/console/")) return { kind: "console" };
  return null;
}

/// A Hubris record's task and what it was: `i2c_driver`, `send → sys op 3`.
function hubrisLine(record) {
  const names = boardValue ? taskNames(boardValue) : [];
  const name = (n) => names[n] ?? `task ${n}`;
  const who = record.task === null || record.task === undefined ? "?" : name(record.task);
  if (record.ch === "SYS") return { who, text: bench.syscallLine(record, name).text };
  if (record.ch === "FAULT") return { who, text: `fault: ${record.name}` };
  return { who: "board", text: record.what };
}

function rowHtml(event) {
  const at = `<span class="at">${event.at.toLocaleString().padStart(14)}</span>`;
  if (event.kind === "hubris") {
    const record = hubris[event.index];
    const { who, text } = hubrisLine(record);
    return `${at}  <span class="who" style="color:${bench.taskColor(record.task)}">${escape(who)}</span> ${escape(text)} <span class="dim">· board ${bench.fmtTime(record.t)}</span>`;
  }
  if (event.kind === "syscall") {
    const t = timeline[event.index];
    const line = (trace[event.index] ?? t.name).replace(/^\[\d+\] /, "");
    return `${at}  <span class="who" style="color:${colorOf(t.pid)}">${escape(nameOf(t.pid))}</span> ${linkify(line)}`;
  }
  const e = exchanges[event.index];
  const pid = timeline[e.syscall]?.pid;
  const who = pid === undefined ? `<span class="who"></span>` : `<span class="who" style="color:${colorOf(pid)}">${escape(nameOf(pid))}</span>`;
  return `${at}  ${who} ${describeExchange(e)}${event.count > 1 ? `  ×${event.count}` : ""}`;
}

function renderEvents(force = false) {
  const list = $("events");
  const filter = filterKey();
  const now = position();
  const from = Math.max(0, now - WINDOW);
  const to = Math.min(events.length, now + WINDOW);
  if (force || from !== rendered.from || to !== rendered.to || filter !== rendered.filter) {
    rendered = { from, to, filter };
    list.innerHTML = "";
    if (from > 0) {
      const elided = document.createElement("div");
      elided.className = "elided";
      elided.textContent = `… ${from.toLocaleString()} earlier`;
      list.appendChild(elided);
    }
    for (let index = from; index < to; index++) {
      const event = events[index];
      const row = document.createElement("div");
      row.className = `event ${event.kind}${(event.kind === "exchange" && exchanges[event.index].error) || (event.kind === "hubris" && hubris[event.index].ch !== "SYS") ? " error" : ""}`;
      row.dataset.at = event.at;
      row.dataset.index = index;
      const pid = event.kind === "syscall" ? timeline[event.index].pid : event.kind === "exchange" ? timeline[exchanges[event.index].syscall]?.pid : undefined;
      if (pid !== undefined) row.style.borderLeftColor = colorOf(pid);
      if (event.kind === "hubris") row.style.borderLeftColor = bench.taskColor(hubris[event.index].task);
      row.innerHTML = rowHtml(event);
      list.appendChild(row);
    }
    if (to < events.length) {
      const elided = document.createElement("div");
      elided.className = "elided";
      elided.textContent = `… ${(events.length - to).toLocaleString()} later`;
      list.appendChild(elided);
    }
  }
  for (const row of list.querySelectorAll(".event")) {
    const index = Number(row.dataset.index);
    row.classList.toggle("past", index <= now);
    row.classList.toggle("now", index === now);
  }
  const marked = list.querySelector(".event.now");
  if (marked) marked.scrollIntoView({ block: "nearest" });
}

// A click on a row seeks to it and opens what it is about; a click on a
// link in the row opens that instead.
$("events").onclick = (click) => {
  const row = click.target.closest(".event");
  if (!row) return;
  const event = events[Number(row.dataset.index)];
  // A Hubris row names an instant of the board's own: stand it there.
  if (event.kind === "hubris") {
    leaveOpening();
    return seekBoard((hubris[event.index].t + 1) * 1000);
  }
  const link = click.target.closest("a.link");
  let focus = focusOf(event);
  if (link) focus = link.dataset.path !== undefined ? { kind: "file", path: link.dataset.path } : { kind: "fd", fd: Number(link.dataset.fd) };
  go(event.at, focus);
};

$("filter").onchange = () => {
  $("custom-filter").hidden = $("filter").value !== "custom";
  rebuildEvents();
  renderEvents(true);
  drawCustomFilter(true);
};

// The custom filter's dropdowns: their options made again from what the
// timeline holds now and then as it grows, and at once when the list is
// built again; never under a dropdown that is open, which a new set of
// options would close.
let customDrawn = 0;
function drawCustomFilter(force = false) {
  const box = $("custom-filter");
  if (box.hidden || (!force && performance.now() - customDrawn < 500)) return;
  customDrawn = performance.now();
  for (const [field, any] of FIELDS) {
    const select = box.querySelector(`[data-field="${field}"]`);
    if (document.activeElement === select && !force) continue;
    const html = [`<option value="">${any}</option>`, ...customOptions.list(field, customSpec[field]).map(([value, count]) => `<option value="${escape(value)}">${escape(value)} (${count.toLocaleString()})</option>`)].join("");
    if (select.shownOptions !== html) {
      select.innerHTML = html;
      select.shownOptions = html;
    }
    select.value = customSpec[field];
    select.classList.toggle("on", !!customSpec[field]);
  }
  const text = box.querySelector('[data-field="text"]');
  if (document.activeElement !== text) text.value = customSpec.text;
  $("custom-shown").textContent = isEmpty(customSpec) ? "" : `${events.length.toLocaleString()} rows`;
}

function chooseCustom(field, value) {
  customSpec = { ...customSpec, [field]: value };
  try {
    localStorage.setItem("timeline-filter", JSON.stringify(customSpec));
  } catch {
    // Kept for this page only.
  }
  rebuildEvents();
  renderEvents(true);
  drawCustomFilter(true);
}

// A page restored with "custom" already chosen shows its dropdowns.
$("custom-filter").hidden = $("filter").value !== "custom";
for (const select of $("custom-filter").querySelectorAll("select")) select.onchange = () => chooseCustom(select.dataset.field, select.value);
let customTyping = null;
$("custom-filter").querySelector('[data-field="text"]').oninput = (event) => {
  clearTimeout(customTyping);
  customTyping = setTimeout(() => chooseCustom("text", event.target.value), 200);
};
$("custom-clear").onclick = () => {
  customSpec = emptySpec();
  chooseCustom("text", "");
};

/// Seeks to `at` and, once there, opens what `focus` names.
function go(at, focus) {
  stop();
  leaveOpening();
  at = clamp(at);
  if (focus?.kind === "file") context.path = focus.path.replace(/^\/+/, "");
  pending = focus ? { at, ...focus } : null;
  seek(at);
}

// ---- the panels --------------------------------------------------------------
//
// One panel per readable pattern of the meta lens. A few patterns have a
// renderer that knows their shape; the rest are shown as the JSON they
// are. Which panels stand in the open, which fold under "the machine" and
// which under "kernel internals" is the one opinion the page keeps.

const OPEN = ["board", "usb", "processes", FILES, DESCRIPTORS, "net"];
const MACHINE = [
  "processes/{pid}/threads/{tid}/registers",
  "processes/{pid}/threads/{tid}/disassembly",
  "processes/{pid}/memory/{address}/{length}",
  "processes/{pid}/maps",
];
const panels = new Map(); // pattern -> { element, body, raw, diff, pathLabel }

function escape(text) {
  return String(text).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function makePanel(pattern, title, into) {
  const element = document.createElement("section");
  element.className = "panel";
  element.dataset.pattern = pattern;
  element.innerHTML = `<h2><span class="path">${escape(title)}</span><span class="tools"><button class="rawtoggle" title="${escape(interface_[pattern]?.read ?? "")}">raw</button></span></h2><div class="body"></div><pre class="raw"></pre><div class="diff"></div>`;
  element.querySelector(".rawtoggle").onclick = (event) => {
    element.classList.toggle("raw");
    event.target.classList.toggle("on");
  };
  into.appendChild(element);
  equipPanel(element);
  const panel = {
    element,
    body: element.querySelector(".body"),
    raw: element.querySelector(".raw"),
    diff: element.querySelector(".diff"),
    pathLabel: element.querySelector(".path"),
  };
  panels.set(pattern, panel);
  return panel;
}

function buildPanels() {
  panels.clear();
  $("panels").innerHTML = "";
  $("machine").querySelector(".grid").innerHTML = "";
  $("internals").querySelector(".grid").innerHTML = "";
  const readable = Object.entries(meta?.paths ?? {})
    // The board's trace is the bench's to read, through its probes.
    .filter(([pattern, lens]) => lens.readable && pattern !== "meta" && !pattern.startsWith("meta/") && !pattern.startsWith("board/"))
    .map(([pattern]) => pattern);
  // A session with the container, when the page was asked for one.
  if (terminalPort) makeTerminalPanel();
  for (const pattern of OPEN) if (readable.includes(pattern)) makePanel(pattern, pattern, $("panels"));
  if (readable.includes("board")) makeMemoryPanel();
  if (readable.includes("board")) makeScreenPanel();
  // The host's side of the boundary, which is not a path of the
  // container's store but is what the page is to it: its edge, its console.
  makeEdgePanel();
  makeConsolePanel();
  for (const pattern of MACHINE) if (readable.includes(pattern)) makePanel(pattern, pattern, $("machine").querySelector(".grid"));
  const rest = readable.filter((pattern) => !OPEN.includes(pattern) && !MACHINE.includes(pattern)).sort();
  for (const pattern of rest) makePanel(pattern, pattern, $("internals").querySelector(".grid"));
  // The path bar's suggestions: every pattern.
  $("paths").innerHTML = readable.map((pattern) => `<option value="${escape(pattern)}">`).join("");
}

// The board's memory, drawn: see `memory.js`. It reads the board's store
// as the board panel is drawn, at the same instant.
let memoryView = null;
function makeMemoryPanel() {
  const element = document.createElement("section");
  element.className = "panel memory-panel";
  element.id = "memory";
  element.innerHTML = `<h2><span class="path">board memory</span> — whose it is, what is written as it runs, and by what</h2><div class="body"></div>`;
  const board = panels.get("board")?.element;
  if (board) board.after(element);
  else $("panels").appendChild(element);
  equipPanel(element);
  memoryView = new MemoryView(element.querySelector(".body"), {
    read,
    query,
    stand: (ps) => seekBoard(ps),
    escape,
    colorOf: bench.taskColor,
    update,
  });
}

// The panel on the board's LTDC, if it has one, and a keyboard for it:
// see `screen.js`. Shown once it has a frame to show, first of all.
// The board's screen: see `screen.js`. Live, the worker sends each new frame
// itself; at an instant, the panel reads the frame there.
let screenView = null;
function makeScreenPanel() {
  const element = document.createElement("section");
  element.className = "panel screen-panel";
  element.id = "screen";
  element.hidden = true;
  element.innerHTML = `<h2><span class="path">screen</span> — the panel on the board's LTDC; click it to type at the board</h2><div class="body"></div>`;
  $("panels").prepend(element);
  equipPanel(element);
  screenView = new ScreenView(element.querySelector(".body"), { read, type: boardControl, live: () => boardTime === null && playing !== null });
}

function makeEdgePanel() {
  const element = document.createElement("section");
  element.className = "panel edge";
  element.id = "edge";
  element.innerHTML = `<h2><span class="path">/iso/net</span> — this page, as the container's peer</h2>
    <form id="send-form">
      <label>port <input id="port" type="number" value="8080" style="width:6em"></label>
      <input id="request" type="text" value="GET / HTTP/1.0\\r\\n\\r\\n">
      <button id="send" type="submit">send</button>
    </form>
    <div id="responses"></div>`;
  $("panels").appendChild(element);
  equipPanel(element);
  $("send-form").onsubmit = (event) => {
    event.preventDefault();
    send(Number($("port").value), $("request").value.replace(/\\n/g, "\n").replace(/\\r/g, "\r"));
  };
}

// ---- the terminal -------------------------------------------------------------
//
// With `&terminal=PORT`, a session with whatever listens on that port in the
// container: one connection, held open, a line at a time. What the guest
// writes appears as it is written — which is as the frontier runs into it,
// so the page plays while a session is open.

const terminalPort = Number(new URLSearchParams(location.search).get("terminal")) || null;
let terminalId = null;
const terminalHistory = [];
let terminalRecall = 0;

function makeTerminalPanel() {
  const element = document.createElement("section");
  element.className = "panel terminal";
  element.id = "terminal";
  // Titled by what is at the other end, once it says (see `terminalWrite`);
  // until then, all the page knows is the port.
  element.innerHTML = `<h2><span class="path" id="term-title">port ${terminalPort}</span><span id="term-sub"> — a session in the container</span><span class="tools"><select id="term-example" title="the Hubris example on the board: choosing another flashes it, with \`flash\` in this session"><option value="">example…</option></select><button id="term-machine" type="button" title="the processes, files and timeline behind this session">show the machine</button></span></h2>
    <pre id="term-out"></pre>
    <form id="term-form"><span class="prompt">›</span><input id="term-in" type="text" autocomplete="off" spellcheck="false" placeholder="humility tasks"></form>`;
  $("panels").appendChild(element);
  equipPanel(element);
  $("term-form").onsubmit = (event) => {
    event.preventDefault();
    const line = $("term-in").value;
    $("term-in").value = "";
    if (line.trim() && terminalHistory[terminalHistory.length - 1] !== line) terminalHistory.push(line);
    if (/^\s*(flash|upload)\b/.test(line)) examplesStale = true;
    terminalRecall = terminalHistory.length;
    // The session does not echo, as a terminal's line discipline would.
    terminalWrite(line + "\n");
    worker.postMessage({ type: "write", id: terminalId, data: line + "\n" });
    if (!playing && finished === null) $("play").click();
  };
  $("term-in").onkeydown = (event) => {
    if (event.key !== "ArrowUp" && event.key !== "ArrowDown") return;
    event.preventDefault();
    terminalRecall = Math.max(0, Math.min(terminalHistory.length, terminalRecall + (event.key === "ArrowUp" ? -1 : 1)));
    $("term-in").value = terminalHistory[terminalRecall] ?? "";
  };
  $("term-machine").onclick = () => leaveOpening();
  $("term-example").onchange = () => {
    const name = $("term-example").value;
    if (!name) return;
    $("term-in").value = `flash ${name}`;
    $("term-form").requestSubmit();
  };
  $("term-out").onclick = () => {
    if (!window.getSelection().toString()) $("term-in").focus();
  };
  // A file dropped on the terminal goes to the session as the door takes
  // one: `upload NAME SIZE`, then its bytes. A Hubris archive dropped so is
  // then there to `flash`.
  element.ondragover = (event) => {
    if (!event.dataTransfer?.types.includes("Files")) return;
    event.preventDefault();
    element.classList.add("dropping");
  };
  element.ondragleave = () => element.classList.remove("dropping");
  element.ondrop = async (event) => {
    event.preventDefault();
    element.classList.remove("dropping");
    for (const file of event.dataTransfer.files) await uploadToTerminal(file);
  };
}

async function uploadToTerminal(file) {
  const bytes = new Uint8Array(await file.arrayBuffer());
  const name = file.name.replace(/\s+/g, "_");
  const head = new TextEncoder().encode(`upload ${name} ${bytes.length}\n`);
  const message = new Uint8Array(head.length + bytes.length);
  message.set(head);
  message.set(bytes, head.length);
  terminalWrite(`upload ${name}  (${bytes.length.toLocaleString()} bytes, dropped)\n`);
  examplesStale = true;
  worker.postMessage({ type: "write", id: terminalId, data: message }, [message.buffer]);
  if (!playing && finished === null) $("play").click();
}

/// A command for the board's clock, through the bench's front door as a
/// typed one would go — so a recording has it — on a session of its own,
/// whose answers nobody reads.
let controlId = null;
function boardControl(line) {
  if (controlId === null) {
    controlId = nextRequest++;
    worker.postMessage({ type: "open", id: controlId, port: terminalPort });
  }
  worker.postMessage({ type: "write", id: controlId, data: line + "\n" });
  if (!playing && finished === null) $("play").click();
}

function openTerminal() {
  terminalId = nextRequest++;
  worker.postMessage({ type: "open", id: terminalId, port: terminalPort });
  $("term-in").focus();
  listExamples();
}

/// The Hubris examples the bench has to flash — its `archives`, asked for
/// on a session of their own — for the terminal's picker; asked again once
/// a `flash` or an `upload` in the terminal has finished (its prompt is back).
let examplesId = null;
let examplesText = "";
let examplesStale = false;
let examples = [];
let examplesOn = null;
function listExamples() {
  if (examplesId !== null || !terminalPort) return;
  examplesId = nextRequest++;
  examplesText = "";
  worker.postMessage({ type: "open", id: examplesId, port: terminalPort });
  worker.postMessage({ type: "write", id: examplesId, data: "archives\nexit\n" });
}

function examplesListed(text) {
  examples = [...text.matchAll(/^(?:\$ )? {4}(\S+) +(\S+) +(\S+) +(\/\S+)$/gm)].map((m) => ({ name: m[1], app: m[2], board: m[3], path: m[4] }));
  examplesOn = text.match(/^(?:\$ )?on the board: (.*)$/m)?.[1] ?? null;
  const select = $("term-example");
  if (!select) return;
  select.innerHTML =
    `<option value="">${examples.length ? "example…" : "no examples"}</option>` +
    examples
      .map((e) => `<option value="${escape(e.name)}">${escape(e.app === "-" ? e.name : e.app)} — ${escape(bench.BOARDS[e.board]?.title ?? (e.board === "-" ? "a board it doesn't name" : e.board))}</option>`)
      .join("");
  markExample();
}

/// The picker shows what is on the board: the archive last flashed, else —
/// at the start, when it is the image's own — the one built for this board.
function markExample() {
  const select = $("term-example");
  if (!select || !examples.length) return;
  const named = examples.filter((e) => e.board === boardValue?.name);
  const on = examples.find((e) => e.path === examplesOn) ?? (named.length === 1 ? named[0] : null);
  if (select.value !== (on?.name ?? "")) select.value = on?.name ?? "";
}

/// What arrives may hold a title for the session — OSC 0 or 2, the escape a
/// program titles its terminal window with: `ESC ] 0 ; title BEL`, or `ST`
/// in place of the BEL. It names the panel and is not printed. A sequence
/// split across two arrivals waits for the rest.
let terminalPending = "";
function terminalWrite(text) {
  text = terminalPending + text;
  terminalPending = "";
  const unfinished = text.lastIndexOf("\x1b]");
  if (unfinished >= 0 && !/[\x07]|\x1b\\/.test(text.slice(unfinished))) {
    terminalPending = text.slice(unfinished);
    text = text.slice(0, unfinished);
  }
  text = text.replace(/\x1b\]([02]);([^\x07\x1b]*)(?:\x07|\x1b\\)/g, (_, kind, title) => {
    $("term-title").textContent = title;
    $("term-sub").textContent = "";
    return "";
  });
  const out = $("term-out");
  out.textContent += text;
  if (examplesStale && /(^|\n)\$ /.test(text)) {
    examplesStale = false;
    listExamples();
  }
  out.scrollTop = out.scrollHeight;
}

// ---- sizing and focus ---------------------------------------------------------
//
// Every panel resizes from its corner, and the others reflow around it; a
// double-click on its heading gives it back its own size. ⤢ focuses one
// panel: that panel alone, at the size of the window, until ⤢ again or Esc.

// The browser writes a resized panel's size into its style. Once it has,
// that size is the panel's: it stops growing and shrinking with its row,
// and the height cap that keeps an unsized panel short no longer applies.
// Watched both ways: a size written that the cap kept the panel from
// taking changes no size, so the resize alone would never be seen.
function keepSize(target) {
  if ((target.style.width || target.style.height) && !target.classList.contains("maximized") && target.style.flex !== "0 0 auto") {
    target.style.flex = "0 0 auto";
    target.style.maxHeight = "none";
  }
}
const resizes = new ResizeObserver((entries) => entries.forEach(({ target }) => keepSize(target)));
const restyles = new MutationObserver((records) => records.forEach(({ target }) => keepSize(target)));
const resized = {
  observe(element) {
    resizes.observe(element);
    restyles.observe(element, { attributes: true, attributeFilter: ["style"] });
  },
};

function equipPanel(element) {
  const heading = element.querySelector("h2");
  let tools = heading.querySelector(".tools");
  if (!tools) {
    tools = document.createElement("span");
    tools.className = "tools";
    heading.appendChild(tools);
  }
  const focus = document.createElement("button");
  focus.type = "button";
  focus.className = "focus";
  focus.textContent = "⤢";
  focus.title = "focus: this panel alone, at the size of the window (Esc to return)";
  focus.onclick = (event) => {
    event.stopPropagation();
    maximize(element.classList.contains("maximized") ? null : element);
  };
  tools.prepend(focus);
  heading.title = "double-click to give the panel back its own size";
  heading.ondblclick = (event) => {
    if (event.target.closest("button")) return;
    for (const property of ["width", "height", "flex", "maxHeight"]) element.style[property] = "";
  };
  resized.observe(element);
}

/// Focuses `element`, or none.
function maximize(element) {
  for (const open of document.querySelectorAll(".panel.maximized")) {
    open.classList.remove("maximized");
    open.querySelector("h2 .focus").textContent = "⤢";
  }
  if (!element) return;
  element.classList.add("maximized");
  element.querySelector("h2 .focus").textContent = "⤡";
  if (element.id === "terminal") $("term-in").focus();
}

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && document.querySelector(".panel.maximized")) maximize(null);
});

function makeConsolePanel() {
  const element = document.createElement("section");
  element.className = "panel";
  element.id = "console";
  element.innerHTML = `<h2><span class="path">/iso/console</span> — what the container wrote</h2><pre id="stdout"></pre><pre id="stderr" style="opacity:.7"></pre>`;
  $("panels").appendChild(element);
  equipPanel(element);
}

/// Any value, as a tree of keys and values.
function renderJson(value, depth = 0) {
  if (value === null || typeof value !== "object") return `<span class="value">${escape(JSON.stringify(value))}</span>`;
  if (Array.isArray(value)) {
    if (!value.length) return "[]";
    if (value.every((v) => v === null || typeof v !== "object")) return escape(JSON.stringify(value));
    return `<div class="kv">${value.map((v, i) => `<div><span class="key">${i}</span>${renderJson(v, depth + 1)}</div>`).join("")}</div>`;
  }
  const entries = Object.entries(value);
  if (!entries.length) return "{}";
  return `<div class="kv">${entries.map(([k, v]) => `<div><span class="key">${escape(k)}</span>${renderJson(v, depth + 1)}</div>`).join("")}</div>`;
}

/// 256 bytes from rsp as quadwords, two a row, little-endian read out.
function hexDump(memory) {
  if (!memory?.bytes) return "";
  const base = BigInt(memory.address);
  const hex = memory.bytes;
  const rows = [];
  for (let byte = 0; byte * 2 < hex.length; byte += 16) {
    const words = [];
    for (let w = 0; w < 2; w++) {
      const from = (byte + w * 8) * 2;
      if (from >= hex.length) break;
      words.push("0x" + hex.slice(from, from + 16).match(/../g).reverse().join("").padStart(16, "0"));
    }
    rows.push(`<div class="word"><span class="address">0x${(base + BigInt(byte)).toString(16)}</span>  ${words.join("  ")}</div>`);
  }
  return rows.join("");
}

/// Bytes as hex, sixteen a row with the printable characters beside.
function hexRows(hex) {
  const rows = [];
  for (let at = 0; at < hex.length; at += 32) {
    const chunk = hex.slice(at, at + 32).match(/../g);
    const printable = chunk.map((h) => {
      const byte = parseInt(h, 16);
      return byte >= 0x20 && byte < 0x7f ? String.fromCharCode(byte) : ".";
    });
    rows.push(`<div class="word"><span class="address">${(at / 2).toString(16).padStart(6, "0")}</span>  ${chunk.join(" ").padEnd(47)}  ${escape(printable.join(""))}</div>`);
  }
  return rows.join("");
}

function browse(path) {
  context.path = path.replace(/\/+/g, "/").replace(/^\//, "");
  pending = { at: current, kind: "file" };
  seek(current);
}

function view(pid, tid = null) {
  context.pid = pid;
  context.tid = tid;
  seek(current);
}

/// The directory a file path is in, with a trailing slash stripped.
function parentOf(path) {
  const trimmed = path.replace(/\/+$/, "");
  const cut = trimmed.lastIndexOf("/");
  return cut <= 0 ? "" : trimmed.slice(0, cut);
}

function processLabel(p) {
  return `<span style="color:${colorOf(p.pid)}">${escape(nameOf(p.pid))}</span> · pid ${p.pid}`;
}

// ---- USB ---------------------------------------------------------------------
//
// The container's USB devices, from its store, and what they carried. A
// CMSIS-DAP probe's transfers are commands and their answers, so the log is
// decoded as those: which command, and for a register transfer, which
// registers it read and wrote.

let granted = []; // the devices the worker holds for the container

const DAP = {
  0x00: "Info", 0x01: "HostStatus", 0x02: "Connect", 0x03: "Disconnect", 0x04: "TransferConfigure",
  0x05: "Transfer", 0x06: "TransferBlock", 0x07: "TransferAbort", 0x08: "WriteABORT", 0x09: "Delay",
  0x0a: "ResetTarget", 0x10: "SWJ_Pins", 0x11: "SWJ_Clock", 0x12: "SWJ_Sequence", 0x13: "SWD_Configure",
  0x1d: "SWD_Sequence", 0x7e: "QueueCommands", 0x7f: "ExecuteCommands", 0xff: "error",
};
const DP = { 0x0: "DPIDR/ABORT", 0x4: "CTRL/STAT", 0x8: "SELECT", 0xc: "RDBUFF" };
const AP = { 0x0: "CSW", 0x4: "TAR", 0xc: "DRW" };
const ACK = ["?", "OK", "WAIT", "?", "FAULT"];

function bytesOf(hex) {
  const out = [];
  for (let i = 0; i + 1 < hex.length; i += 2) out.push(parseInt(hex.slice(i, i + 2), 16));
  return out;
}

/// One transfer as a CMSIS-DAP command or answer: a name, and a summary.
function decodeDap(transfer) {
  const b = bytesOf(transfer.head);
  const name = DAP[b[0]] ?? `0x${(b[0] ?? 0).toString(16)}`;
  const outward = !transfer.endpoint.startsWith("8");
  if (!outward) {
    if (b[0] === 0x05) return { name, summary: `${b[1]} done, ${ACK[b[2] & 7] ?? b[2]}` };
    if (b[0] === 0x06) return { name, summary: `${b[1] | (b[2] << 8)} done, ${ACK[b[3] & 7] ?? b[3]}` };
    return { name, summary: `${transfer.length} byte${transfer.length === 1 ? "" : "s"}` };
  }
  if (b[0] === 0x05) {
    const parts = [];
    let at = 3;
    for (let n = 0; n < b[2] && at < b.length; n++) {
      const request = b[at++];
      const reads = request & 2;
      const label = (request & 1 ? AP : DP)[request & 0x0c] ?? `0x${(request & 0x0c).toString(16)}`;
      parts.push(`${request & 1 ? "AP" : "DP"} ${reads ? "read" : "write"} ${label}`);
      if (!reads || request & 0x10) at += 4;
    }
    return { name, summary: parts.join(", ") + (b[2] > parts.length ? ` … (${b[2]})` : "") };
  }
  if (b[0] === 0x06) {
    const request = b[4] ?? 0;
    return { name, summary: `${b[2] | (b[3] << 8)} × ${request & 1 ? "AP" : "DP"} ${request & 2 ? "read" : "write"} ${(request & 1 ? AP : DP)[request & 0x0c] ?? ""}` };
  }
  if (b[0] === 0x00) return { name, summary: { 1: "vendor", 2: "product", 3: "serial", 4: "version", 0xf0: "capabilities", 0xfe: "packet count", 0xff: "packet size" }[b[1]] ?? `id 0x${(b[1] ?? 0).toString(16)}` };
  return { name, summary: `${transfer.length} byte${transfer.length === 1 ? "" : "s"}` };
}

async function connectUsb() {
  try {
    await requestUsb();
    worker.postMessage({ type: "usb" });
  } catch (why) {
    // Cancelling the chooser is an answer, not an error.
    if (!/No device selected/i.test(String(why))) status(`USB: ${why}`);
  }
}

const renderers = {
  // The USB devices the container's programs can open, and their traffic.
  usb(value, panel) {
    const enumerated = new Set(value.devices.map((d) => d.id));
    const can = !!navigator.usb;
    let html = `<div class="usb"><div class="usbbar"><button class="connect" ${can ? "" : "disabled"} title="${can ? "grant the container a USB device from this computer — a debug probe, say; programs started from now on find it" : "this browser has no WebUSB"}">connect a USB device…</button>`;
    const waiting = granted.filter((g) => !enumerated.has(g.id));
    if (waiting.length) html += ` <span class="dim">${waiting.map((g) => escape(g.name || g.id)).join(", ")}: granted — the next program to start finds it</span>`;
    html += `</div>`;
    if (!value.devices.length) html += `<div class="dim">no USB devices${can ? "" : " — and this browser cannot grant one"}</div>`;
    for (const d of value.devices) {
      const kind = d.provider === "board" ? "the board's probe" : "from this computer";
      const rows = d.recent
        .slice(-14)
        .reverse()
        .map((t) => {
          const { name, summary } = decodeDap(t);
          const outward = !t.endpoint.startsWith("8");
          return `<tr class="${outward ? "out" : "in"}"><td class="at">${t.at.toLocaleString()}</td><td>${outward ? "→" : "←"} ${escape(t.endpoint)}</td><td><b>${escape(name)}</b></td><td>${escape(summary)}</td>${t.status ? `<td class="refusal">${t.status}</td>` : ""}</tr>`;
        })
        .join("");
      html += `<div class="device"><div><b>${escape(d.name ?? d.id)}</b> · ${escape(d.vendor ?? "")}:${escape(d.product ?? "")} · ${kind} · bus ${d.bus} device ${d.address}${d.open ? ` · open ×${d.open}` : ""}</div>
        <div class="dim">${escape(d.interface ?? "")} · ${escape(d.speed)} Mbit/s · out ${d.out.transfers.toLocaleString()} transfers, ${(d.out.bytes / 1024).toFixed(1)} KiB · in ${d.in.transfers.toLocaleString()}, ${(d.in.bytes / 1024).toFixed(1)} KiB</div>
        ${rows ? `<table class="transfers">${rows}</table>` : `<div class="dim">nothing carried yet</div>`}</div>`;
    }
    panel.body.innerHTML = html + `</div>`;
    panel.body.querySelector(".connect").onclick = connectUsb;
  },

  // A board on the container's bench: its LEDs drawn, and the core's state.
  board(value, panel) {
    const leds = value.leds ?? (value.led_pin ? [{ pin: value.led_pin, color: "green", state: value.led }] : []);
    // What the core's addresses are — which task, which function or
    // stack — as the board's archive names them.
    const NAMED = ["pc", "lr", "sp", "msp", "psp"];
    const registers = Object.entries(value.registers ?? {})
      .map(([name, held]) => {
        const what = NAMED.includes(name) ? whatis(value, held) : null;
        return `<div><span class="key">${escape(name)}</span>${escape(held)}${what ? ` <span class="dim">${escape(what.text)}</span>` : ""}</div>`;
      })
      .join("");
    const at = value.registers?.pc ? whatis(value, value.registers.pc) : null;
    const where = at?.owner
      ? `in <b>${escape(at.owner === "kernel" ? "the kernel" : at.owner)}</b>${at.symbol ? ` · ${escape(at.symbol.name)}${at.symbol.offset ? ` + 0x${at.symbol.offset.toString(16)}` : ""}` : ""}${at.owner !== "kernel" && at.task !== null ? ` <span class="dim">task ${at.task}</span>` : ""}`
      : "";
    const exceptions = Object.entries(value.exceptions ?? {})
      .map(([name, count]) => `${escape(name)} ${count.toLocaleString()}`)
      .join(" · ");
    const lamps = leds
      .map((led) => `<div class="led ${escape(led.color)}${led.state === "on" ? " on" : ""}" title="${escape(led.pin)}: ${escape(led.state ?? "never driven")}"></div>`)
      .join("");
    const lit = leds.map((led) => `${escape(led.pin)} ${escape(led.state ?? "never driven")}`).join(", ");
    const heard = (value.heard ?? []).slice(-4).map((line) => `<div class="dim">heard ${escape(line)}</div>`).join("");
    const wired = (value.wired ?? []).map((line) => `<div class="dim">wired ${escape(line)}</div>`).join("");
    // The board's clock, and — on a bench with a front door to send them
    // through — the controls for it.
    const hz = (f) => (!f ? "off" : f % 1e6 === 0 ? `${f / 1e6} MHz` : f >= 1e6 ? `${(f / 1e6).toFixed(3)} MHz` : `${f / 1e3} kHz`);
    const clocks = value.clocks
      ? `core ${hz(value.clocks.core)} · AHB ${hz(value.clocks.hclk)} · APB ${[...new Set(value.clocks.pclk)].map(hz).join(" / ")}`
      : "";
    const speeds = ["0.1×", "1×", "10×", "max"]
      .map((s) => `<button type="button" data-clock="speed ${s.replace("×", "")}"${value.speed === s ? ' class="on"' : ""}>${s}</button>`)
      .join("");
    const controls = terminalPort
      ? `<span class="controls">${speeds}
          <button type="button" data-clock="${value.paused ? "resume" : "pause"}">${value.paused ? "resume" : "pause"}</button>
          <button type="button" data-clock="advance 1ms" title="move the board on by a millisecond">+1 ms</button>
          <button type="button" data-clock="advance 100ms">+100 ms</button>
          <button type="button" data-clock="advance next" title="to whatever is due next: SysTick, a timer, a transfer">next event</button></span>`
      : "";
    const clockText = value.speed ? `${value.paused ? "<b>paused</b> · " : ""}speed ${escape(value.speed)} · managing ${(value.ratio ?? 0).toFixed(3)}× real time` : "";
    // What the USARTs sent, the tail of it, as a serial console shows it.
    const serial = value.serial ? `<div class="key">serial</div><pre class="serial">${escape(value.serial.slice(-2000))}</pre>` : "";
    panel.element.classList.add("bench-panel");
    // A skeleton made once, and each part of it replaced only when what it
    // shows changes: the board isn't redrawn under the pointer on every read.
    if (!panel.body.querySelector(".board > .drawing")) {
      panel.body.innerHTML = `<div class="board"><div class="instant"></div><div class="drawing"></div><div class="status"></div><div class="where"></div><div class="clock"><span class="clock-text"></span> <span class="clock-controls"></span></div><div class="dim clock-tree"></div>
        <div class="timeline"><div class="timeline-head"></div><canvas class="timeline-canvas"></canvas><div class="timeline-readout dim">&nbsp;</div></div>
        <div class="values"></div><div class="probes"></div>
        <details class="more"><summary>LEDs, faults, the network, the core's registers</summary><div class="more-body"></div></details></div>`;
      const more = panel.body.querySelector("details.more");
      more.open = moreOpen;
      more.ontoggle = () => (moreOpen = more.open);
    }
    const fill = (selector, html) => {
      const element = panel.body.querySelector(selector);
      update(element, html);
      return element;
    };
    fill(".drawing", value.gpio ? bench.drawBoard(value, new Set(probes)) : `<div class="leds">${lamps}</div>`);
    fill(".where", where);
    fill(".status", `<b>${escape(value.chip)}</b>${value.core ? ` (${escape(value.core)})` : ""} · <b>${escape(value.state)}</b> · ${(value.simulated_ms / 1000).toFixed(3)} s simulated`);
    // The clock's figures change on every read; its buttons only when the
    // speed or the pause does, so a press on one isn't lost to a redraw.
    // Standing at an instant of the board's: where, and steps in its time.
    // The clock's controls are the live board's, so they wait till then.
    const steps = [
      [-1e8, "−100 µs"],
      [1e7, "+10 µs"],
      [1e8, "+100 µs"],
      [1e9, "+1 ms"],
    ]
      .map(([by, label]) => `<button type="button" data-board-step="${by}">${label}</button>`)
      .join("") +
      `<button type="button" data-follow${followValues ? ' class="on"' : ""} title="stand here again, following where the board's values go on the way: what in memory came in from outside, and why">follow values</button>`;
    fill(
      ".instant",
      boardTime === null
        ? ""
        : `<b>standing at ${bench.fmtTime(value.time_ps / 1000)}</b> of the board's time, every panel as it was then <span class="controls">${steps}<button type="button" data-board-live class="on">back to live</button></span>`,
    );
    showValues(panel, value);
    fill(".clock-text", clockText);
    fill(".clock-controls", value.speed && boardTime === null ? controls : "");
    fill(".clock-tree", value.speed ? clocks : "");
    const layerButtons = Object.keys(bench.LAYERS)
      .map((layer) => `<button type="button" data-layer="${layer}"${timelineLayers[layer] ? ' class="on"' : ""}>${layer}</button>`)
      .join("");
    const spanButtons = [1e6, 1e7, 1e8, 1e9]
      .map((span) => `<button type="button" data-span="${span}"${span === timelineSpan ? ' class="on"' : ""}>${bench.fmtSpan(span)}</button>`)
      .join("");
    fill(".timeline-head", `<b>timeline</b> <span class="dim">in the board's time</span> <span class="controls">${layerButtons}</span> <span class="controls">${spanButtons}</span>`);
    showProbes(panel.body.querySelector(".probes"), value);
    const transcript = fill(".more-body", `<div>LED${leds.length > 1 ? "s" : ""} ${lit || "none"} · ${value.faults} faults · debug probes: USB CMSIS-DAP, GDB on ${escape(value.probe)}${value.debugger ? " (attached)" : ""}</div>
      ${wired}
      ${heard}
      <div class="dim">${escape(value.image)} · ${value.instructions.toLocaleString()} instructions · ${escape(value.mode)} mode, ${value.privileged ? "privileged" : "unprivileged"}</div>
      <div class="dim">${exceptions}</div>
      ${serial}
      <div class="registers">${registers}</div>`).querySelector(".serial");
    if (transcript) transcript.scrollTop = transcript.scrollHeight;
    panel.body.onclick = (event) => {
      const button = event.target.closest("[data-clock]");
      if (button) return boardControl(button.dataset.clock);
      const at = event.target.closest("[data-board-time]");
      // Just after the record: its time is whole nanoseconds, the board's
      // picoseconds.
      if (at) return seekBoard((Number(at.dataset.boardTime) + 1) * 1000);
      const step = event.target.closest("[data-board-step]");
      if (step) return seekBoard((boardTime ?? value.time_ps) + Number(step.dataset.boardStep));
      if (event.target.closest("[data-board-live]")) return backToLive();
      if (event.target.closest("[data-follow]")) {
        followValues = !followValues;
        return seekBoard(boardTime ?? value.time_ps);
      }
      const why = event.target.closest("[data-why]");
      if (why) {
        valuesWhy = valuesWhy === why.dataset.why ? null : why.dataset.why;
        return showValues(panel, value);
      }
      const layer = event.target.closest("[data-layer]");
      if (layer) {
        timelineLayers[layer.dataset.layer] = !timelineLayers[layer.dataset.layer];
        saveTimeline();
        timelineRecords.clear();
        timelineCursor = null;
        return renderers.board(value, panel);
      }
      const span = event.target.closest("[data-span]");
      if (span) {
        timelineSpan = Number(span.dataset.span);
        saveTimeline();
        return renderers.board(value, panel);
      }
      const target = event.target.closest("[data-probe], [data-part], [data-unprobe], [data-window]");
      if (!target) return;
      if (target.dataset.window) probeWindow = Number(target.dataset.window);
      else toggleProbe(target.dataset.probe ?? target.dataset.part ?? target.dataset.unprobe);
      renderers.board(value, panel);
    };
    boardValue = value;
    boardPanel = panel;
    if (boardTime === null && value.time_ps !== undefined) liveBoardTime = value.time_ps;
    markExample();
    memoryView?.update(value, boardTime);
    equipTimeline(panel);
    drawTimeline();
    followTimeline();
    followTrace();
  },

  processes(value, panel, state) {
    const s = values.statistics?.value;
    const running = s?.current;
    let html = s ? `<div style="opacity:.7;margin-bottom:4px">retired ${s.retired.toLocaleString()} · in bytecode ${s.accelerated.toLocaleString()} · running ${escape(nameOf(running))} (pid ${running})</div>` : "";
    for (const p of value.processes) {
      const state_ = typeof p.state === "string" ? p.state : JSON.stringify(p.state);
      html += `<div class="process${p.pid === running ? " current" : ""}${p.pid === state.pid ? " viewing" : ""}" data-pid="${p.pid}" title="view this process's paths" style="border-left-color:${colorOf(p.pid)}"><b>${processLabel(p)}</b> · parent ${p.parent ? `${escape(nameOf(p.parent))} (${p.parent})` : "none"} · ${escape(state_)}${p.displaced ? ` · ${p.displaced} pages displaced` : ""}${p.pid === state.pid ? " · in view" : ""}` +
        p.threads.map((t) => `<div class="thread${t.tid === state.tid && p.pid === state.pid ? " viewing" : ""}" data-pid="${p.pid}" data-tid="${t.tid}">tid ${t.tid} @ ${t.rip} · ${escape(t.state)} · retired ${t.retired.toLocaleString()}</div>`).join("") +
        `</div>`;
    }
    panel.body.innerHTML = html;
    for (const box of panel.body.querySelectorAll(".process")) box.onclick = (event) => {
      const thread = event.target.closest(".thread");
      view(Number(box.dataset.pid), thread ? Number(thread.dataset.tid) : null);
    };
  },

  [FILES](value, panel) {
    const path = value.path;
    const parts = path.split("/").filter(Boolean);
    let crumb = `<div class="crumb"><a data-path="">/</a>`;
    parts.forEach((part, i) => {
      crumb += `<a data-path="${escape(parts.slice(0, i + 1).join("/"))}">${escape(part)}</a><span class="sep">/</span>`;
    });
    crumb += `<span style="opacity:.6"> · cwd ${escape(value.cwd ?? "?")}</span></div>`;
    let html = crumb;
    if (value.kind === "directory") {
      html += `<table><tr><th>name</th><th>kind</th><th>size</th></tr>`;
      if (path !== "/") html += `<tr class="entry directory"><td><a data-path="${escape(parentOf(path))}">..</a></td><td></td><td></td></tr>`;
      for (const entry of value.entries) {
        const target = (path === "/" ? "" : path.replace(/^\//, "") + "/") + entry.name;
        html += `<tr class="entry ${entry.kind}"><td><a data-path="${escape(target)}">${escape(entry.name)}</a></td><td>${entry.kind}</td><td>${entry.kind === "file" ? entry.size.toLocaleString() : ""}</td></tr>`;
      }
      html += `</table>`;
      if (value.truncated) html += `<div class="elided">… and more: the listing is capped</div>`;
    } else if (value.kind === "file") {
      html += `<div class="filehead">file · ${value.size.toLocaleString()} bytes · mode ${value.mode}${value.truncated ? " · showing the beginning" : ""}</div>`;
      if (value.text !== undefined) html += `<pre>${escape(value.text)}</pre>`;
      else html += hexRows(value.hex);
    } else if (value.kind === "symlink") {
      const target = value.target.startsWith("/") ? value.target : parentOf(path) + "/" + value.target;
      html += `<div>symlink → <a class="link" data-path="${escape(target)}">${escape(value.target)}</a></div>`;
    } else html += `<div>${escape(value.kind)}</div>`;
    panel.body.innerHTML = html;
    for (const link of panel.body.querySelectorAll("a[data-path]")) link.onclick = () => browse(link.dataset.path);
  },

  [DESCRIPTORS](value, panel) {
    let html = `<table><tr><th>fd</th><th>what</th><th>path</th><th>offset</th><th>flags</th></tr>`;
    for (const d of value) {
      html += `<tr data-fd="${d.fd}"><td>${d.fd}</td><td>${escape(d.what)}</td><td>${d.path ? `<a class="link" data-path="${escape(d.path)}">${escape(d.path)}</a>` : ""}</td><td>${d.offset}</td><td>${d.flags}${d.cloexec ? " cloexec" : ""}</td></tr>`;
    }
    panel.body.innerHTML = html + "</table>";
    for (const link of panel.body.querySelectorAll("a[data-path]")) link.onclick = () => browse(link.dataset.path);
  },

  net(value, panel) {
    if (!value.sockets.length) {
      panel.body.innerHTML = `<div class="refusal">no sockets</div>`;
      return;
    }
    panel.body.innerHTML = value.sockets.map((s) => {
      let state;
      if (s.state === "idle") state = "idle";
      else if (s.state.bound) state = `bound ${s.state.bound}`;
      else if (s.state.listening) state = `listening on ${s.state.listening.address} · backlog ${s.state.listening.backlog} · ${s.state.listening.queued} waiting to be accepted`;
      else if (s.state.connected) {
        const c = s.state.connected;
        state = `connected ${c.local} ↔ ${c.peer} · ${c.receive_queued} bytes to read, ${c.transmit_queued} to send${c.read_shut ? " · read shut" : ""}${c.write_shut ? " · write shut" : ""}`;
      } else state = escape(JSON.stringify(s.state));
      const holders = s.holders.map((pid) => `<span style="color:${colorOf(pid)}">${escape(nameOf(pid))}</span> (${pid})`).join(", ") || "nobody";
      return `<div class="socket" data-socket="${s.id}" data-edge="${s.edge ?? ""}"><span class="id">socket${s.id}</span> ${s.family} ${s.kind === 1 ? "stream" : s.kind === 2 ? "dgram" : "kind " + s.kind}${s.edge !== null ? ` · <b>edge conn/${s.edge}</b>` : ""} · held by ${holders}<div class="thread">${state}</div></div>`;
    }).join("");
  },

  "processes/{pid}/threads/{tid}/registers"(r, panel, state) {
    const names_ = ["rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "rip", "fs_base"];
    panel.body.innerHTML = `<div class="registers"><div><span class="name">thread</span><span class="value">${escape(nameOf(state.pid))} pid ${state.pid} tid ${state.tid}</span></div>` +
      names_.map((n) => `<div><span class="name">${n}</span><span class="value">${r[n]}</span></div>`).join("") +
      `<div><span class="name">flags</span><span class="value ${r.flags_stale ? "stale" : ""}">${r.flags}${r.flags_stale === true ? " (stale: a later instruction overwrote them before anything read them)" : r.flags_stale === null ? " (freshness unknown at this stop)" : ""}</span></div></div>`;
  },

  "processes/{pid}/threads/{tid}/disassembly"(lines, panel) {
    disassembled = lines.length;
    panel.body.innerHTML = lines.length
      ? lines.map((line, i) => `<div class="line${i === 0 ? " rip" : ""}"><span>${line.address}</span><span class="bytes">${line.bytes.match(/../g).join(" ")}</span><span>${escape(line.text)}</span></div>`).join("")
      : "<i>nothing executable at rip, or the module was baked without disassembly</i>";
  },

  "processes/{pid}/memory/{address}/{length}"(memory, panel) {
    panel.body.innerHTML = `<div id="stack">${hexDump(memory)}</div>`;
  },

  "processes/{pid}/maps"(maps, panel) {
    panel.body.innerHTML = `<pre id="maps">${escape(maps)}</pre>`;
  },

  "processes/{pid}/mapped"(value, panel) {
    panel.body.innerHTML = `<pre>${value.ranges.map(([a, b]) => `${a} – ${b}`).join("\n")}</pre>`;
  },
};

/// Which concrete path of the last state a pattern was read at.
function concreteOf(pattern) {
  return Object.keys(values).find((path) => values[path].pattern === pattern);
}

function render(state) {
  current = state.at;
  if (live && current > frontier) {
    frontier = current;
    $("slider").max = frontier;
  }
  values = state.values;
  learnNames(values.processes?.value?.processes);
  $("slider").value = current;
  $("at").textContent = current.toLocaleString();
  $("took").textContent = live && current === frontier ? "the frontier" : state.restored ? `restored in ${Math.round(state.restored)} ms` : "";
  for (const [pattern, panel] of panels) {
    const path = concreteOf(pattern);
    const held = path ? values[path] : null;
    panel.pathLabel.textContent = path ?? pattern;
    if (!held) {
      panel.body.innerHTML = `<div class="refusal">nothing to read here at this instant</div>`;
      panel.raw.textContent = "";
    } else if (held.error !== undefined) {
      panel.body.innerHTML = `<div class="refusal">${escape(held.error)}</div>`;
      panel.raw.textContent = held.error;
      if (pattern === FILES) {
        panel.body.innerHTML += ` <a class="link" data-path="">back to /</a>`;
        panel.body.querySelector("a").onclick = () => browse("");
      }
    } else {
      const renderer = renderers[pattern];
      if (renderer) renderer(held.value, panel, state);
      else panel.body.innerHTML = renderJson(held.value);
      panel.raw.textContent = JSON.stringify(held.value, null, 1);
    }
    renderDiff(pattern, panel, path, held);
  }
  $("stdout").textContent = state.stdout;
  $("stderr").textContent = state.stderr + (state.log ? "\n" + state.log : "");
  renderEvents();
  applyFocus(state);
}

// ---- probes on the board -------------------------------------------------------
//
// What the board panel's probes are on — pins, buses, the serial line, the
// cable, parts — and what each has gathered from the board's trace. The
// trace is read a slice at a time, from the record after the last one
// read; a probe added reads the latest few thousand first. A board whose
// trace is behind where the page last read — the view moved back in
// time — starts again from its latest.

const probes = [];
const probeRecords = new Map(); // channel -> records, oldest first
let traceCursor = null;
let traceBusy = false;
let probeWindow = 2e9; // ns of simulated time a pin's waveform shows
let moreOpen = false;
let boardValue = null;
let boardPanel = null;
const RECORDS_KEPT = 5000;

function toggleProbe(name) {
  const at = probes.indexOf(name);
  if (at >= 0) probes.splice(at, 1);
  else probes.unshift(name);
  probeRecords.clear();
  traceCursor = null;
}

function probeChannels() {
  return [...new Set(probes.map(bench.channelOf).filter(Boolean))];
}

/// Shows `html` in `element`, changing only what differs from what it
/// shows now: an element whose tag is the same stays, its attributes and
/// text brought up to date, so what the pointer is on isn't swapped for a
/// copy between a press and its release — which would lose the click.
function update(element, html) {
  if (element.shown === html) return;
  const template = document.createElement("template");
  template.innerHTML = html;
  morph(element, template.content);
  element.shown = html;
}

function morph(into, from) {
  const had = [...into.childNodes];
  const wanted = [...from.childNodes];
  wanted.forEach((node, n) => {
    const old = had[n];
    if (!old) return into.appendChild(node);
    if (old.nodeType !== node.nodeType || old.nodeName !== node.nodeName) return into.replaceChild(node, old);
    if (node.nodeType !== Node.ELEMENT_NODE) {
      if (old.nodeValue !== node.nodeValue) old.nodeValue = node.nodeValue;
      return;
    }
    for (const { name } of [...old.attributes]) if (!node.hasAttribute(name)) old.removeAttribute(name);
    for (const { name, value } of node.attributes) if (old.getAttribute(name) !== value) old.setAttribute(name, value);
    morph(old, node);
  });
  for (const old of had.slice(wanted.length)) old.remove();
}

/// What an address of the board's is (`board/whatis`), kept by image and
/// address — an image's archive does not change under it — and asked for
/// the first time it is wanted, the board redrawn when it is known.
const whatisKept = new Map();
function whatis(value, address) {
  const key = `${value.image} ${address}`;
  if (whatisKept.has(key)) return whatisKept.get(key);
  whatisKept.set(key, null);
  read(`board/whatis/${address}`)
    .then((answer) => {
      if (!answer?.text) return;
      whatisKept.set(key, answer);
      if (boardPanel && boardValue) renderers.board(boardValue, boardPanel);
    })
    .catch(() => {});
  return null;
}

// ---- the board's timeline ------------------------------------------------------
//
// A strip in the board's own time, ending where the board stands: which
// Hubris task ran, what it asked the kernel, the exceptions taken, the
// buses' and the cable's traffic; each layer switched on or off. Hovering
// says what an event was; a click stands every panel there.

let timelineLayers = { tasks: true, syscalls: true, interrupts: false, devices: true };
let timelineSpan = 1e7;
try {
  const kept = JSON.parse(localStorage.getItem("zaqaru.timeline") ?? "null");
  if (kept?.layers) timelineLayers = { ...timelineLayers, ...kept.layers };
  if (kept?.span) timelineSpan = kept.span;
} catch {
  // A browser that keeps nothing: the defaults.
}
function saveTimeline() {
  try {
    localStorage.setItem("zaqaru.timeline", JSON.stringify({ layers: timelineLayers, span: timelineSpan }));
  } catch {
    // Not kept, then.
  }
}
const timelineRecords = new Map();
const TIMELINE_KEPT = 40000;
let timelineCursor = null;
let timelineBusy = false;

/// The channels the layers that are on are made of, and the board's own.
function timelineChannels() {
  return [...Object.entries(bench.LAYERS).filter(([layer]) => timelineLayers[layer]).flatMap(([, channels]) => channels), "BOARD"];
}

/// The task table's names, by index, from `board/tasks`: asked for once an
/// image, and the timeline redrawn when they come.
const tasksKept = new Map();
function taskNames(value) {
  if (tasksKept.has(value.image)) return tasksKept.get(value.image);
  tasksKept.set(value.image, []);
  read("board/tasks")
    .then((answer) => {
      tasksKept.set(value.image, (answer?.tasks ?? []).map((task) => task.name));
      drawTimeline();
    })
    .catch(() => {});
  return [];
}

/// Reads what the timeline's channels have carried since the last read,
/// as the probes do: from the last so many, then on from there, and from
/// scratch when the view has moved earlier.
async function followTimeline() {
  if (timelineBusy || !boardPanel) return;
  timelineBusy = true;
  let again = false;
  try {
    const since = timelineCursor ?? -5000;
    const answer = await read(`board/trace/${since}/${timelineChannels().join(",")}`);
    if (!answer || answer.error !== undefined) return;
    if (timelineCursor !== null && answer.next < timelineCursor) {
      timelineRecords.clear();
      timelineCursor = null;
      again = true;
      return;
    }
    for (const record of answer.records) {
      if (!timelineRecords.has(record.ch)) timelineRecords.set(record.ch, []);
      timelineRecords.get(record.ch).push(record);
    }
    // The oldest let go once a read, not once a record.
    for (const list of timelineRecords.values()) if (list.length > TIMELINE_KEPT) list.splice(0, list.length - TIMELINE_KEPT);
    timelineCursor = answer.next;
    if (answer.records.length || since < 0) drawTimeline();
  } catch {
    // Nothing to read at this instant.
  } finally {
    timelineBusy = false;
    if (again) followTimeline();
  }
}

const KIND_COLORS = {
  send: "#58a6ff", kernel: "#f0a020", reply: "#3fb950", recv: "#8b949e", fault: "#ff5d5d", other: "#bc8cff",
  tick: "#8888", irq: "#d29922", device: "#39c5cf", board: "#ff5d5d",
};
const LANE = 15;
const LABELS = 76;

/// Draws the strip: the window ending where the board stands, its lanes,
/// and what the pointer can find in them.
function drawTimeline() {
  const canvas = boardPanel?.body.querySelector(".timeline-canvas");
  if (!canvas || !boardValue) return;
  const names = taskNames(boardValue);
  const name = (n) => names[n] ?? `task ${n}`;
  const end = (boardTime ?? boardValue.time_ps ?? 0) / 1000;
  const from = end - timelineSpan;
  const { lanes, board } = bench.timelineLanes(timelineRecords, timelineLayers, from, end + 1, name);
  const width = canvas.clientWidth || 600;
  const height = Math.max(1, lanes.length) * LANE + 14;
  const scale = window.devicePixelRatio || 1;
  canvas.style.height = `${height}px`;
  if (canvas.width !== Math.round(width * scale) || canvas.height !== Math.round(height * scale)) {
    canvas.width = Math.round(width * scale);
    canvas.height = Math.round(height * scale);
  }
  const g = canvas.getContext("2d");
  g.setTransform(scale, 0, 0, scale, 0, 0);
  g.clearRect(0, 0, width, height);
  const plot = width - LABELS;
  const x = (t) => LABELS + ((t - from) / timelineSpan) * plot;
  const hits = [];
  g.font = "10px ui-monospace, monospace";
  g.textBaseline = "middle";
  lanes.forEach((lane, i) => {
    const y = i * LANE;
    g.fillStyle = "#8881";
    g.fillRect(LABELS, y + 1, plot, LANE - 2);
    g.fillStyle = "#888";
    g.fillText(lane.name, 2, y + LANE / 2);
    for (const span of lane.spans) {
      const a = Math.max(LABELS, x(span.from));
      const b = Math.min(width, x(span.to));
      if (b <= a) continue;
      g.fillStyle = span.color;
      g.fillRect(a, y + 2, Math.max(1, b - a), LANE - 4);
      if (b - a > 34) {
        g.save();
        g.beginPath();
        g.rect(a, y, b - a - 2, LANE);
        g.clip();
        g.fillStyle = "#fff";
        g.fillText(span.label, a + 3, y + LANE / 2);
        g.restore();
      }
      hits.push({ lane: i, a, b, t: span.from, text: `${span.label} runs` });
    }
    for (const mark of lane.marks) {
      const at = x(mark.t);
      g.fillStyle = KIND_COLORS[mark.kind] ?? "#888";
      const tall = mark.kind === "fault" || mark.kind === "kernel";
      g.fillRect(at, y + (tall ? 1 : 3), 1.5, LANE - (tall ? 2 : 6));
      hits.push({ lane: i, a: at - 3, b: at + 3, t: mark.t, text: mark.text, mark: true });
    }
  });
  for (const mark of board) {
    g.fillStyle = KIND_COLORS.board;
    g.fillRect(x(mark.t), 0, 2, lanes.length * LANE);
    hits.push({ lane: -1, a: x(mark.t) - 3, b: x(mark.t) + 3, t: mark.t, text: mark.text, mark: true });
  }
  g.fillStyle = "#888";
  const axis = lanes.length * LANE + 7;
  g.fillText(`−${bench.fmtSpan(timelineSpan)}`, LABELS, axis);
  const here = boardTime === null ? "now" : "here";
  g.fillText(`${bench.fmtTime(end)} ${here}`, Math.max(LABELS, width - 150), axis);
  if (!lanes.length) {
    g.fillText("no layer on", LABELS + 4, LANE / 2);
  }
  canvas.hits = hits;
  canvas.window = { from, plot };
}

/// The pointer on the strip: says what is under it; a click stands the
/// board there — at the event, if one is under it, or at that time.
function equipTimeline(panel) {
  const canvas = panel.body.querySelector(".timeline-canvas");
  if (!canvas || canvas.equipped) return;
  canvas.equipped = true;
  const found = (event) => {
    const box = canvas.getBoundingClientRect();
    const px = event.clientX - box.left;
    const lane = Math.floor((event.clientY - box.top) / LANE);
    const under = (canvas.hits ?? []).filter((h) => (h.lane === lane || h.lane === -1) && px >= h.a && px <= h.b);
    // A mark before a span: it is the narrower thing to have aimed at.
    const hit = under.find((h) => h.mark) ?? under[0];
    const t = canvas.window ? canvas.window.from + ((px - LABELS) / canvas.window.plot) * timelineSpan : null;
    return { hit, t };
  };
  const readout = () => panel.body.querySelector(".timeline-readout");
  canvas.onmousemove = (event) => {
    const { hit, t } = found(event);
    const line = hit ? `${bench.fmtTime(hit.t)}  ${hit.text}` : t !== null && t > 0 ? `${bench.fmtTime(t)}` : "";
    readout().textContent = line || "\u00a0";
  };
  canvas.onmouseleave = () => (readout().textContent = "\u00a0");
  canvas.onclick = (event) => {
    const { hit, t } = found(event);
    const at = hit ? hit.t + 1 : t;
    if (at !== null && at > 0) seekBoard(at * 1000);
  };
}

// ---- where values came from -------------------------------------------------------
//
// Standing at an instant with values followed, the board panel lists what
// of its memory came in from outside — runs of bytes, where each is, and
// the read of a peripheral's data register it traces back to — and, for
// the one asked about, why: each copy back to the byte on the wire, every
// step's time a handle on that instant.

let valuesKept = { at: null, tagged: null, why: new Map() };
let valuesWhy = null;

function showValues(panel, value) {
  const box = panel.body.querySelector(".values");
  if (!box) return;
  if (boardTime === null || !followValues) return update(box, "");
  if (valuesKept.at !== boardTime) {
    valuesKept = { at: boardTime, tagged: null, why: new Map() };
    read("board/tagged")
      .then((tagged) => {
        if (valuesKept.at === boardTime) valuesKept.tagged = tagged;
        showValues(panel, boardValue ?? value);
      })
      .catch(() => {});
  }
  if (valuesWhy && !valuesKept.why.has(valuesWhy)) {
    const asked = valuesWhy;
    valuesKept.why.set(asked, null);
    read(`board/why/${asked}`)
      .then((why) => {
        valuesKept.why.set(asked, why);
        showValues(panel, boardValue ?? value);
      })
      .catch(() => {});
  }
  const tagged = valuesKept.tagged;
  if (!tagged) return update(box, `<div class="dim">following where values go, on the way here…</div>`);
  if (!tagged.following) return update(box, `<div class="dim">not following: no board at the checkpoint this was stood from</div>`);
  const rows = tagged.shown
    .map((run) => {
      const origin = run.origin ? `← ${escape(run.origin.register)} 0x${run.origin.value.toString(16)} at ${bench.stamp(run.origin.t)}` : "";
      const open = valuesWhy === String(run.addr);
      const chain = open ? whyHtml(valuesKept.why.get(String(run.addr))) : "";
      return `<div class="run${open ? " open" : ""}"><button type="button" data-why="${run.addr}">${open ? "hide" : "why"}</button> <code>${run.hex}</code>${run.length > 16 ? "…" : ""} ${escape(run.where)} <span class="dim">${origin}</span>${chain}</div>`;
    })
    .join("");
  update(
    box,
    `<div class="values-head"><b>values from outside</b> <span class="dim">— ${tagged.runs} run${tagged.runs === 1 ? "" : "s"} of memory hold what came in through a peripheral since ${bench.fmtTime(tagged.since_ns)}</span></div>${rows || `<div class="dim">nothing from outside is in memory now</div>`}`,
  );
}

/// A `why` as a chain: newest first, each step a handle on its instant.
function whyHtml(why) {
  if (!why) return `<div class="why dim">asking…</div>`;
  const steps = (list) =>
    list
      .map((step) => {
        const what =
          step.what === "store"
            ? `stored <code>0x${step.value.toString(16)}</code> at ${escape(step.where)}`
            : step.what === "read"
              ? `read <code>0x${step.value.toString(16)}</code> from <b>${escape(step.register)}</b>${step.record ? ` — the byte on the wire at ${bench.stamp(step.record.t)}` : ""}`
              : "merged with";
        const other = step.other?.length ? `<div class="why-other">${steps(step.other)}</div>` : "";
        return `<div class="why-step">${bench.stamp(step.t)} ${what} <span class="dim">by ${escape(step.code)}</span>${other}</div>`;
      })
      .join("");
  return `<div class="why">${steps(why.steps) || `<div class="dim">came from nowhere followed</div>`}</div>`;
}

/// The probes' cards, one for each probe, kept from read to read: a card's
/// head is redrawn only when it changes, its body as its records come — so
/// a press on its × or its zoom isn't lost to a redraw.
function showProbes(box, value) {
  const show = update;
  if (!probes.length) {
    show(box, `<div class="dim">Probe something: click a pin, a bus, a part, the Ethernet jack or the serial port on the board above.</div>`);
    return;
  }
  if (box.shown !== undefined) {
    box.innerHTML = "";
    box.shown = undefined;
  }
  const cards = new Map([...box.children].map((card) => [card.dataset.name, card]));
  for (const [name, card] of cards) if (!probes.includes(name)) card.remove();
  const windows = [1e6, 1e7, 1e8, 1e9, 1e10]
    .map((w) => `<button type="button" data-window="${w}"${w === probeWindow ? ' class="on"' : ""}>${bench.fmtSpan(w)}</button>`)
    .join("");
  probes.forEach((name, n) => {
    let card = cards.get(name);
    if (!card) {
      card = document.createElement("div");
      card.className = "probe";
      card.dataset.name = name;
      card.innerHTML = `<div class="probe-head"></div><div class="probe-body"></div>`;
    }
    if (box.children[n] !== card) box.insertBefore(card, box.children[n] ?? null);
    const kind = bench.probeKind(name);
    const records = probeRecords.get(bench.channelOf(name) ?? "") ?? [];
    const label = { pin: "pin", i2c: "I2C bus", spi: "SPI bus", usart: "serial line", eth: "Ethernet cable", part: "part" }[kind];
    show(card.firstChild, `<b>${escape(name)}</b> <span class="dim">${label}</span>
        ${kind === "pin" ? `<span class="controls">${windows}</span>` : ""}
        <button type="button" class="unprobe" data-unprobe="${escape(name)}" title="take the probe off">×</button>`);
    show(card.lastChild, bench.probeBody(name, records, value, probeWindow));
  });
}

/// Reads what the probes' channels have carried since the last read, and
/// shows it.
async function followTrace() {
  const channels = probeChannels();
  if (!channels.length || traceBusy) return;
  traceBusy = true;
  let again = false;
  try {
    const since = traceCursor ?? -2000;
    const answer = await read(`board/trace/${since}/${channels.join(",")}`);
    if (answer?.error !== undefined || !answer) return;
    if (traceCursor !== null && answer.next < traceCursor) {
      // The view is earlier than the last read — a seek into the past, or
      // to an instant of the board's — so what the probes hold is partly
      // its future: start again from there, now, since a view standing
      // still asks for nothing more.
      probeRecords.clear();
      traceCursor = null;
      again = true;
      return;
    }
    for (const record of answer.records) {
      if (!probeRecords.has(record.ch)) probeRecords.set(record.ch, []);
      const list = probeRecords.get(record.ch);
      list.push(record);
      if (list.length > RECORDS_KEPT) list.splice(0, list.length - RECORDS_KEPT);
    }
    const reread = since < 0;
    traceCursor = answer.next;
    if ((answer.records.length || reread) && boardPanel && boardValue) {
      const box = boardPanel.body.querySelector(".probes");
      if (box) showProbes(box, boardValue);
    }
  } catch {
    // Nothing to read at this instant: the board is not there yet.
  } finally {
    traceBusy = false;
    if (again) followTrace();
  }
}

// ---- focus -------------------------------------------------------------------
//
// What the clicked row was about, opened once the machine stands at its
// instant: a file browses to it; a descriptor browses to its file, or
// lights up its socket, or its row; a connection lights up its socket.

function highlight(element) {
  if (!element) return;
  element.classList.add("focused");
  const details = element.closest("details");
  if (details) details.open = true;
  element.scrollIntoView({ block: "nearest" });
}

function applyFocus(state) {
  for (const element of document.querySelectorAll(".panel.focused")) element.classList.remove("focused");
  if (!pending || pending.at !== state.at) return;
  const focus = pending;
  pending = null;
  if (focus.kind === "file") highlight(panels.get(FILES)?.element);
  else if (focus.kind === "console") highlight($("console"));
  else if (focus.kind === "edge") {
    const panel = panels.get("net");
    highlight(panel?.element);
    const socket = panel?.body.querySelector(`.socket[data-edge="${focus.edge}"]`);
    if (socket) {
      socket.classList.add("hit");
      socket.scrollIntoView({ block: "nearest" });
    }
  } else if (focus.kind === "fd") {
    const descriptors = values[concreteOf(DESCRIPTORS)]?.value ?? [];
    const d = descriptors.find((d) => d.fd === focus.fd);
    if (!d) return;
    if (d.path) {
      // The descriptor names a file: browse to it, and come back here.
      context.path = d.path.replace(/^\/+/, "");
      pending = { at: state.at, kind: "file" };
      seek(state.at);
      return;
    }
    const socket = d.what.match(/^socket(\d+)$/);
    if (socket) {
      const panel = panels.get("net");
      highlight(panel?.element);
      const row = panel?.body.querySelector(`.socket[data-socket="${socket[1]}"]`);
      if (row) {
        row.classList.add("hit");
        row.scrollIntoView({ block: "nearest" });
      }
    } else {
      const panel = panels.get(DESCRIPTORS);
      highlight(panel?.element);
      const row = panel?.body.querySelector(`tr[data-fd="${focus.fd}"]`);
      if (row) row.classList.add("hit");
    }
  }
}

// ---- pin and diff --------------------------------------------------------------
//
// Values are structured, so "what changed between two instants" is a diff
// of two JSON values: keys added, keys gone, leaves changed. Arrays whose
// elements carry an identity — a pid, an fd, a socket id, a name — are
// matched by it, so a descriptor closed in the middle shows as the one
// descriptor gone rather than as every later one changed.

const IDENTITIES = ["pid", "tid", "fd", "id", "name", "address"];

function identityOf(array) {
  if (!array.length || !array.every((v) => v && typeof v === "object" && !Array.isArray(v))) return null;
  return IDENTITIES.find((key) => array.every((v) => v[key] !== undefined)) ?? null;
}

function diff(before, after, at = "", out = []) {
  if (out.length > 200) return out;
  if (before === after) return out;
  const kind = (v) => (Array.isArray(v) ? "array" : v === null ? "null" : typeof v);
  if (kind(before) !== kind(after) || kind(after) !== "object" && kind(after) !== "array") {
    if (JSON.stringify(before) !== JSON.stringify(after)) out.push({ at, before, after });
    return out;
  }
  if (kind(after) === "array") {
    const key = identityOf(before) ?? identityOf(after);
    if (key && (identityOf(before) === key || !before.length) && (identityOf(after) === key || !after.length)) {
      const was = new Map(before.map((v) => [v[key], v]));
      const is = new Map(after.map((v) => [v[key], v]));
      for (const [id, v] of was) if (!is.has(id)) out.push({ at: `${at}[${key}=${id}]`, before: v, after: undefined });
      for (const [id, v] of is) {
        if (!was.has(id)) out.push({ at: `${at}[${key}=${id}]`, before: undefined, after: v });
        else diff(was.get(id), v, `${at}[${key}=${id}]`, out);
      }
      return out;
    }
    const length = Math.max(before.length, after.length);
    for (let i = 0; i < length; i++) diff(before[i], after[i], `${at}[${i}]`, out);
    return out;
  }
  for (const key of new Set([...Object.keys(before), ...Object.keys(after)])) diff(before[key], after[key], at ? `${at}.${key}` : key, out);
  return out;
}

function short(value) {
  if (value === undefined) return "";
  const text = JSON.stringify(value);
  return text.length > 60 ? text.slice(0, 57) + "…" : text;
}

function renderDiff(pattern, panel, path, held) {
  if (!pinned) {
    panel.diff.innerHTML = "";
    return;
  }
  const then = pinned.values[path];
  if (!path || !then || !held) {
    const was = Object.keys(pinned.values).find((p) => pinned.values[p].pattern === pattern);
    panel.diff.innerHTML = `<div class="none">since ${pinned.at.toLocaleString()}: ${was ? `was ${escape(was)} then` : "not read then"}</div>`;
    return;
  }
  const changes = diff(then.value ?? then.error, held.value ?? held.error);
  if (!changes.length) {
    panel.diff.innerHTML = `<div class="none">since ${pinned.at.toLocaleString()}: unchanged</div>`;
    return;
  }
  panel.diff.innerHTML = `<div style="opacity:.7">since ${pinned.at.toLocaleString()}: ${changes.length} change${changes.length === 1 ? "" : "s"}</div>` + changes.slice(0, 40).map((c) => {
    if (c.before === undefined) return `<div class="add">+ ${escape(c.at)} ${escape(short(c.after))}</div>`;
    if (c.after === undefined) return `<div class="remove">− ${escape(c.at)} ${escape(short(c.before))}</div>`;
    return `<div class="change">~ ${escape(c.at)}: ${escape(short(c.before))} → ${escape(short(c.after))}</div>`;
  }).join("") + (changes.length > 40 ? `<div class="elided">… ${changes.length - 40} more</div>` : "");
}

function pin() {
  if (pinned) {
    pinned = null;
    $("pin").textContent = "pin";
    $("pinned").textContent = "";
  } else {
    pinned = { at: current, values: JSON.parse(JSON.stringify(values)) };
    $("pin").textContent = "unpin";
    $("pinned").textContent = `pinned at ${current.toLocaleString()}; panels show what changed since`;
  }
  for (const [pattern, panel] of panels) {
    const path = concreteOf(pattern);
    renderDiff(pattern, panel, path, path ? values[path] : null);
  }
}
$("pin").onclick = pin;

// ---- the path bar --------------------------------------------------------------

$("pathbar").onsubmit = async (event) => {
  event.preventDefault();
  const path = $("path").value.trim().replace(/^\/+/, "");
  if (!path) return;
  $("answer").textContent = `reading ${path} at ${current.toLocaleString()}…`;
  try {
    const value = await read(path);
    $("answer").textContent = `${path} at ${current.toLocaleString()}:\n${JSON.stringify(value, null, 1)}`;
  } catch (why) {
    $("answer").textContent = `${path} at ${current.toLocaleString()}: ${why}`;
  }
};

// ---- controls ----------------------------------------------------------------

function stop() {
  if (playing && live) worker.postMessage({ type: "pause" });
  playing = null;
  $("play").textContent = "play";
}

$("slider").oninput = (event) => {
  stop();
  leaveOpening();
  seek(Number(event.target.value));
};
$("play").onclick = () => {
  if (playing) return stop();
  if (live && finished !== null) return;
  playing = Number($("speed").value);
  $("play").textContent = "pause";
  if (live) worker.postMessage({ type: "run" });
  else seek(current + playing);
};
$("budget").onchange = sendCheckpointing;
$("every").onchange = () => live && sendCheckpointing();
$("back").onclick = () => go(current - 1, null);
$("forward").onclick = () => { stop(); leaveOpening(); if (live && current === frontier) advance(1); else seek(current + 1); };
$("prev").onclick = () => {
  const now = position();
  if (now >= 0 && events[now].at === current && now > 0) go(events[now - 1].at, focusOf(events[now - 1]));
  else if (now >= 0) go(events[now].at, focusOf(events[now]));
};
$("next").onclick = () => {
  const after = position() + 1;
  if (after < events.length) go(events[after].at, focusOf(events[after]));
};
window.addEventListener("resize", drawLanes);

function send(port, request) {
  const id = nextRequest++;
  worker.postMessage({ type: "request", id, port, request });
  const box = document.createElement("div");
  box.className = "exchange-box";
  box.id = `exchange-${id}`;
  box.innerHTML = `<div class="meta">#${id} → port ${port}: ${escape(JSON.stringify(request))}</div>`;
  $("responses").prepend(box);
  if (!playing && finished === null) $("play").click();
  return id;
}

async function loadFiles(moduleFile, tapeFile, snapshotFile, every) {
  $("status").textContent = "loading…";
  const module = await moduleFile.arrayBuffer();
  const tape = tapeFile ? await tapeFile.arrayBuffer() : null;
  const snapshot = snapshotFile ? await snapshotFile.arrayBuffer() : null;
  const publish = $("publish").value.split(",").map((s) => Number(s.trim())).filter(Boolean);
  const budget = Number($("budget").value) / 100;
  worker.postMessage({ type: "load", module, tape, snapshot, checkpointEvery: every, budget, publish }, [module, tape, snapshot].filter(Boolean));
}
$("load").onclick = () => {
  const module = $("module").files[0];
  if (module) loadFiles(module, $("tape").files[0] ?? null, $("snapshot").files[0] ?? null, Number($("every").value));
  else $("status").textContent = "choose a module, and a tape to replay or none to run live";
};

// For a test driving the page from outside.
window.zaqaruDebug = {
  seek,
  advance,
  send,
  read,
  browse,
  view,
  pin,
  go,
  get current() {
    return current;
  },
  get origin() {
    return origin;
  },
  get frontier() {
    return frontier;
  },
  get total() {
    return total;
  },
  get busy() {
    return busy;
  },
  get live() {
    return live;
  },
  get responses() {
    return responses;
  },
  get disassembled() {
    return disassembled;
  },
  get syscalls() {
    return timeline.length;
  },
  get exchanges() {
    return exchanges.length;
  },
  get values() {
    return values;
  },
  get meta() {
    return meta;
  },
  get context() {
    return context;
  },
  get pinned() {
    return pinned;
  },
  get pending() {
    return pending;
  },
  get spans() {
    return spans;
  },
  get names() {
    return Object.fromEntries(names);
  },
  get opening() {
    return document.body.classList.contains("opening");
  },
};

// `?module=…&tape=…` replays from URLs on the same origin; `?module=…&live=8080`
// runs live with those ports published; `&snapshot=…` starts from a booted
// container; `&every=…` sets the checkpoint interval, and `&budget=…` the
// most of a live run's time, in percent, checkpoints may take.
const params = new URLSearchParams(location.search);
if (params.get("module")) {
  $("status").textContent = "fetching…";
  if (params.get("live") !== null) $("publish").value = params.get("live");
  const every = Number(params.get("every") ?? (params.get("snapshot") ? 20000000 : $("every").value));
  $("every").value = every;
  if (params.get("budget")) $("budget").value = params.get("budget");
  const fetchOptional = (name) => (params.get(name) ? fetch(params.get(name)).then((r) => (r.ok ? r.blob() : Promise.reject(`${params.get(name)}: ${r.status}`))) : Promise.resolve(null));
  Promise.all([fetch(params.get("module")).then((r) => r.blob()), fetchOptional("tape"), fetchOptional("snapshot")])
    .then(([m, t, s]) => loadFiles(m, t, s, every))
    .catch((why) => ($("status").textContent = String(why)));
}
