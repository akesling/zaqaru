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
let events = []; // the two merged, as the timeline shows them
let spans = []; // { sent, answered } per request answered through the edge
let playing = null;
let checkpointing = null; // live: what checkpoints are costing, from the worker
let lastState = 0; // when the last state was drawn, by the page's clock
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
  if (busy) {
    queued = { follow: true };
    return;
  }
  busy = true;
  worker.postMessage({ type: "seek", at: null, context });
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
  else if (next.follow) follow();
  else advance(next.advance);
  return true;
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
    $("slider").max = frontier;
    status(`${message.listening.length ? `listening on ${message.listening.join(", ")}` : ""}${message.idle && finished === null ? " · idle" : ""}`);
    rebuildEvents();
    renderEvents(true);
    drawLanes();
    // Whether the panels were following the frontier, before a finish stops the play.
    const following = playing !== null;
    if (finished !== null) stop();
    if (message.reply) {
      if (!drain()) seek(frontier);
    } else if (following && (message.stopped || finished !== null)) follow();
    else if (following && !busy && performance.now() - lastState >= FOLLOW_MS) follow();
    return;
  }
  // The USB devices the worker holds for the container: the page's grants.
  if (message.type === "usb") {
    granted = message.devices;
    const panel = panels.get("usb");
    if (panel && values.usb?.value) renderers.usb(values.usb.value, panel);
    return;
  }
  if (message.type === "state") {
    busy = false;
    lastState = performance.now();
    render(message);
    if (!drain() && playing && !live) seek(current + playing);
    return;
  }
  if (message.type === "stream") {
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

function rebuildEvents() {
  const filter = $("filter").value;
  const raw = filter === "raw";
  events = [];
  // An exchange made during a syscall comes before the syscall's own row:
  // the row is what the call returned, written as it returned.
  if (filter !== "exchange") for (let i = 0; i < timeline.length; i++) events.push({ at: timeline[i].at, kind: "syscall", index: i, order: 1, count: 1 });
  if (filter !== "syscall") {
    // Folded: repeats of one path at one stamp — an idle container reads
    // the clock at every wake — become one row saying how many; polls that
    // found nothing are left out. "Everything" shows each as it was.
    let stamp = -1;
    let groups = new Map(); // op|path -> the row folding it, within one stamp
    for (let i = 0; i < exchanges.length; i++) {
      const e = exchanges[i];
      if (!raw && emptyPoll(e)) continue;
      if (e.syscall !== stamp) {
        stamp = e.syscall;
        groups = new Map();
      }
      const key = `${e.op}|${e.path}`;
      const folded = !raw && !e.error ? groups.get(key) : undefined;
      if (folded) {
        folded.count++;
        continue;
      }
      const row = { at: timeline[e.syscall]?.at ?? frontier, kind: "exchange", index: i, order: 0, count: 1 };
      events.push(row);
      if (!raw && !e.error) groups.set(key, row);
    }
  }
  events.sort((a, b) => a.at - b.at || a.order - b.order || a.index - b.index);
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

function rowHtml(event) {
  const at = `<span class="at">${event.at.toLocaleString().padStart(14)}</span>`;
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
  const filter = $("filter").value;
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
      row.className = `event ${event.kind}${event.kind === "exchange" && exchanges[event.index].error ? " error" : ""}`;
      row.dataset.at = event.at;
      row.dataset.index = index;
      const pid = event.kind === "syscall" ? timeline[event.index].pid : timeline[exchanges[event.index].syscall]?.pid;
      if (pid !== undefined) row.style.borderLeftColor = colorOf(pid);
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
  const link = click.target.closest("a.link");
  let focus = focusOf(event);
  if (link) focus = link.dataset.path !== undefined ? { kind: "file", path: link.dataset.path } : { kind: "fd", fd: Number(link.dataset.fd) };
  go(event.at, focus);
};

$("filter").onchange = () => {
  rebuildEvents();
  renderEvents(true);
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
    .filter(([pattern, lens]) => lens.readable && pattern !== "meta" && !pattern.startsWith("meta/"))
    .map(([pattern]) => pattern);
  // A session with the container, when the page was asked for one.
  if (terminalPort) makeTerminalPanel();
  for (const pattern of OPEN) if (readable.includes(pattern)) makePanel(pattern, pattern, $("panels"));
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
  element.innerHTML = `<h2><span class="path" id="term-title">port ${terminalPort}</span><span id="term-sub"> — a session in the container</span><span class="tools"><button id="term-machine" type="button" title="the processes, files and timeline behind this session">show the machine</button></span></h2>
    <pre id="term-out"></pre>
    <form id="term-form"><span class="prompt">›</span><input id="term-in" type="text" autocomplete="off" spellcheck="false" placeholder="humility tasks"></form>`;
  $("panels").appendChild(element);
  equipPanel(element);
  $("term-form").onsubmit = (event) => {
    event.preventDefault();
    const line = $("term-in").value;
    $("term-in").value = "";
    if (line.trim() && terminalHistory[terminalHistory.length - 1] !== line) terminalHistory.push(line);
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
  worker.postMessage({ type: "write", id: terminalId, data: message }, [message.buffer]);
  if (!playing && finished === null) $("play").click();
}

function openTerminal() {
  terminalId = nextRequest++;
  worker.postMessage({ type: "open", id: terminalId, port: terminalPort });
  $("term-in").focus();
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
    const registers = Object.entries(value.registers ?? {})
      .map(([name, held]) => `<div><span class="key">${escape(name)}</span>${escape(held)}</div>`)
      .join("");
    const exceptions = Object.entries(value.exceptions ?? {})
      .map(([name, count]) => `${escape(name)} ${count.toLocaleString()}`)
      .join(" · ");
    const lamps = leds
      .map((led) => `<div class="led ${escape(led.color)}${led.state === "on" ? " on" : ""}" title="${escape(led.pin)}: ${escape(led.state ?? "never driven")}"></div>`)
      .join("");
    const lit = leds.map((led) => `${escape(led.pin)} ${escape(led.state ?? "never driven")}`).join(", ");
    const heard = (value.heard ?? []).slice(-4).map((line) => `<div class="dim">heard ${escape(line)}</div>`).join("");
    // What the USARTs sent, the tail of it, as a serial console shows it.
    const serial = value.serial ? `<div class="key">serial</div><pre class="serial">${escape(value.serial.slice(-2000))}</pre>` : "";
    panel.body.innerHTML = `<div class="board">
      <div class="leds">${lamps}</div>
      <div><b>${escape(value.chip)}</b>${value.core ? ` (${escape(value.core)})` : ""} · <b>${escape(value.state)}</b> · ${(value.simulated_ms / 1000).toFixed(3)} s simulated</div>
      <div>LED${leds.length > 1 ? "s" : ""} ${lit || "none"} · ${value.faults} faults · probes: USB CMSIS-DAP, GDB on ${escape(value.probe)}${value.debugger ? " (attached)" : ""}</div>
      ${heard}
      <div class="dim">${escape(value.image)} · ${value.instructions.toLocaleString()} instructions · ${escape(value.mode)} mode, ${value.privileged ? "privileged" : "unprivileged"}</div>
      <div class="dim">${exceptions}</div>
      ${serial}
      <div class="registers">${registers}</div>
    </div>`;
    const transcript = panel.body.querySelector(".serial");
    if (transcript) transcript.scrollTop = transcript.scrollHeight;
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
