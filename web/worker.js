// The debugger's engine room: owns the container, the checkpoints and the
// timeline, and answers the page's requests. Runs in a Worker so that
// re-executing a few million instructions never freezes the page.
//
// Three ways to load. With a tape, the whole run is replayed once up front,
// checkpointing as it goes, and every instant is then a seek. Without one —
// live — the container runs against JavaScript's own clock and entropy and
// an edge the page can send requests through; everything the host answers
// is recorded, the frontier advances — on its own while the page plays,
// never waiting on the page to draw — checkpoints are taken on the way,
// and a seek behind the frontier restores a checkpoint and re-executes
// against the recording.
//
// A checkpoint costs what comparing memory costs, a few milliseconds to a
// few tens, and a seek re-executes from the one before it. So live
// checkpoints are spaced by two settings: at least `every` instructions
// apart, and far enough apart in run time that taking them costs at most
// `budget` of it. The progress reports say what that is costing. With a snapshot file (see
// `snapshot.js`), the live run starts from a container somebody already
// booted, and its history begins there.
//
// What a seek hands back is not a fixed set of panels: the worker reads
// the container's `meta` lens once, and at every instant reads every path
// it declares that the instant can fill — `{pid}` and `{tid}` from the
// process and thread in view, `{path}` from the page's browsing, the
// memory under `rsp`. A path the kernel adds appears on the page with no
// change here.
//
// Messages in:  { type: "load", module, tape | null, snapshot | null, checkpointEvery, publish }
//               { type: "advance", by }                 live: run the frontier on by `by`, and report
//               { type: "run" } / { type: "pause" }      live: run the frontier on until paused
//               { type: "checkpointing", every, budget } live: how far apart checkpoints are
//               { type: "seek", at, context, board }    context: { pid, tid, path }; board: { time, follow },
//                                                       picoseconds of the board's own time, in place of `at`;
//                                                       `follow`: where its values go followed on the way
//               { type: "read", id, path }              one path, at the instant in view
//               { type: "query", id, board, path }      one path, at an instant of the board's stood at
//                                                       with board: { time, watch, back, follow }
//               { type: "request", id, port, request }  live: through the edge
//               { type: "open", id, port }               live: a connection that stays open
//               { type: "write", id, data }              live: more for an open connection, a string or bytes
// Messages out: { type: "loaded", ... }  { type: "progress", ... }
//               { type: "state", ... }   { type: "value", id, path, value | error }
//               { type: "display", value }   live: the board's screen, a new frame of it
//               { type: "response", id, response }
//               { type: "stream", id, data | ended }      what an open connection's guest wrote
//               { type: "error", message }

import { Container, Edge, KIND, MountTable, parseTape, standardMounts, text } from "./zaqaru.js";
import { Checkpoints } from "./checkpoints.js";
import { decode, inflate, refill } from "./snapshot.js";
import { WebUsb } from "./usb.js";

let module = null;
let usb = null; // the WebUSB store, when the browser has WebUSB
let tape = null;
let live = null; // the frontier container, in live mode
let edge = null;
let checkpoints = null;
let checkpointEvery = 2000000;
let budget = 0.1; // live: the most of run time checkpoints may take
let running = false; // live: the frontier is running on its own
let ran = 0; // live: ms spent running the container
let ranSinceCheckpoint = 0; // the same, since the last checkpoint
let viewer = null; // the container standing at the last seek
let origin = 0; // where history begins: 0, or a snapshot's instant
let inflated = 0; // ms spent inflating the snapshot
let frontier = 0;
let finished = null;
let timelineSeen = 0; // bytes of the timeline sink already reported
let traceSeen = 0;
let observed = null; // the mount table's exchange log
let observedSeen = 0; // entries of it already reported
let hubrisSeen = 0; // the board's trace records already reported (`board/trace` numbering)
let meta = null; // the store's meta lens: { paths: { pattern: { readable, writable } } }

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function replayMounts() {
  const table = standardMounts();
  table.replay(tape);
  return table;
}

function parseTimeline(textSoFar) {
  return textSoFar
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const [at, pid, tid, name] = line.split(" ");
      return { at: Number(at), pid: Number(pid), tid: Number(tid), name };
    });
}

/// The board's OS-level records since last asked — its Hubris tasks'
/// system calls, which task ran, faults, resets and halts — each stamped
/// with the container's instant it was collected at (`at`), for the
/// overall timeline. Empty with no board.
const HUBRIS_CHANNELS = "SYS,TASK,FAULT,BOARD";
function hubrisGrowth(container) {
  const out = [];
  try {
    for (let reads = 0; reads < 20; reads++) {
      const answer = container.value(`board/trace/${hubrisSeen}/${HUBRIS_CHANNELS}`);
      out.push(...answer.records);
      const moved = answer.next !== hubrisSeen;
      hubrisSeen = answer.next;
      if (!moved || answer.records.length < 5000) break;
    }
  } catch {
    // No board, or none yet.
  }
  return out;
}

/// What the container has logged since last asked: new timeline entries,
/// trace lines, and exchanges across the boundary.
function growth(container) {
  const timelineText = text(container.readback(["iso", "log", "timeline"]) ?? new Uint8Array());
  const traceText = text(container.readback(["iso", "log", "debug"]) ?? new Uint8Array());
  const timeline = parseTimeline(timelineText.slice(timelineSeen));
  const trace = traceText.slice(traceSeen).split("\n").filter(Boolean);
  timelineSeen = timelineText.length;
  traceSeen = traceText.length;
  const exchanges = observed ? observed.slice(observedSeen) : [];
  observedSeen = observed ? observed.length : 0;
  return { timeline, trace, exchanges, hubris: hubrisGrowth(container) };
}

function console_(container, stream) {
  return text(container.readback(["iso", "console", stream]) ?? new Uint8Array());
}

/// The store's declaration of itself: the meta lens for what is readable,
/// the manifest for what each path means.
function describe(container) {
  meta = container.value("meta");
  let manifest = null;
  try {
    manifest = JSON.parse(container.manifest());
  } catch {
    manifest = null;
  }
  return { meta, interface: manifest?.paths ?? {} };
}

async function load({ module: moduleBytes, tape: tapeBytes, snapshot: snapshotBytes, checkpointEvery: every, budget: asked, publish }) {
  const started = performance.now();
  module = await WebAssembly.compile(moduleBytes);
  checkpointEvery = every ?? 2000000;
  budget = asked ?? budget;
  checkpoints = new Checkpoints();
  timelineSeen = 0;
  traceSeen = 0;
  observedSeen = 0;
  hubrisSeen = 0;
  finished = null;
  origin = 0;
  if (tapeBytes) {
    tape = parseTape(new Uint8Array(tapeBytes));
    live = null;
    const mounts = replayMounts();
    observed = mounts.observe();
    const first = await Container.instantiate(module, mounts);
    first.step(0);
    keep(0, first);
    let target = checkpointEvery;
    let total = 0;
    let diffing = 0;
    for (;;) {
      const turn = first.step(target);
      total = first.value("statistics").retired;
      if (turn.kind === KIND.FINISHED) {
        finished = turn.status;
        break;
      }
      const began = performance.now();
      keep(total, first);
      diffing += performance.now() - began;
      target = total + checkpointEvery;
    }
    frontier = total;
    const { timeline, trace, exchanges } = growth(first);
    viewer = first;
    postMessage({
      type: "loaded",
      live: false,
      origin: 0,
      total,
      frontier,
      finished,
      timeline,
      trace,
      exchanges,
      ...describe(first),
      output: console_(first, "stdout"),
      bytecode: tape.bytecode,
      checkpoints: Array.from({ length: checkpoints.length }, (_, i) => checkpoints.at(i)),
      held: checkpoints.held,
      naive: checkpoints.naive,
      diffing,
      loading: performance.now() - started,
    });
    return;
  }
  // Live: the world is JavaScript's, and recorded.
  tape = null;
  edge = new Edge(publish ?? []);
  // And USB devices the page has been granted, if the browser has WebUSB.
  usb = globalThis.navigator?.usb ? new WebUsb({ onArrival: wake, onChange: (devices) => postMessage({ type: "usb", devices }) }) : null;
  await usb?.refresh();
  if (snapshotBytes) {
    const inflating = performance.now();
    const file = decode(await inflate(new Uint8Array(snapshotBytes)));
    inflated = performance.now() - inflating;
    const mounts = MountTable.load(file.mounts, { edge });
    if (usb) mounts.mount(["services", "usb"], usb);
    mounts.record();
    observed = mounts.observe();
    live = await Container.continueFrom(module, file, mounts);
    if (file.refill) refill(live);
    origin = file.at;
    // The container has run, so its clock and its logs are consulted from
    // here: whatever the file kept of the console is the boot's output.
  } else {
    const mounts = standardMounts({ seed: null, config: { trace: 1 }, edge });
    if (usb) mounts.mount(["services", "usb"], usb);
    mounts.record();
    observed = mounts.observe();
    live = await Container.instantiate(module, mounts);
    live.step(0);
  }
  const statistics = live.value("statistics");
  if (statistics.retired !== origin) throw `the container stands at ${statistics.retired}, not the snapshot's ${origin}`;
  keep(origin, live);
  frontier = origin;
  viewer = live;
  growth(live); // the boot's logs, if any were kept, are not the timeline
  postMessage({
    type: "loaded",
    live: true,
    origin,
    total: origin,
    frontier: origin,
    finished: null,
    timeline: [],
    trace: [],
    exchanges: [],
    ...describe(live),
    output: console_(live, "stdout"),
    bytecode: true,
    checkpoints: [origin],
    held: checkpoints.held,
    naive: checkpoints.naive,
    diffing: 0,
    published: publish ?? [],
    listening: [...edge.listening],
    loading: performance.now() - started,
    inflated,
  });
}

/// Live: whether a checkpoint is due at the frontier: `every` instructions
/// since the last, and enough run time since it that the last one's cost is
/// within the budget.
function checkpointDue() {
  const last = checkpoints.entries.at(-1);
  if (frontier < last.at + checkpointEvery) return false;
  const cost = checkpoints.estimate ?? 0;
  return budget >= 1 || cost <= (budget / (1 - budget)) * ranSinceCheckpoint;
}

/// Live: whether a checkpoint is due by the board's time — `BOARD_EVERY`
/// of it since the last, within the same budget — looked at most every
/// `BOARD_LOOK_MS`. An idle container retires nothing while its board runs
/// on, so by its count alone a board left running would have no checkpoint
/// after the one it began with, and everything asked of its past would
/// re-execute it from there.
const BOARD_EVERY = 2.5e11;
const BOARD_LOOK_MS = 100;
let boardLooked = 0;
function boardCheckpointDue() {
  const now = performance.now();
  if (now - boardLooked < BOARD_LOOK_MS) return false;
  boardLooked = now;
  const last = checkpoints.entries.at(-1);
  let board;
  try {
    board = live.value("board").time_ps;
  } catch {
    return false;
  }
  if (typeof board !== "number" || board < (last.board ?? 0) + BOARD_EVERY) return false;
  const cost = checkpoints.estimate ?? 0;
  return budget >= 1 || cost <= (budget / (1 - budget)) * ranSinceCheckpoint;
}

/// How close together the checkpoints taken by the board's time are kept,
/// by how far behind the latest they are: `[within, apart]`, picoseconds of
/// the board's. Near the present, where most is asked, every one; further
/// back, fewer, so that what is kept stays bounded as the run goes on.
const THINNED = [
  [1e13, BOARD_EVERY],
  [1.2e14, 2e12],
  [Infinity, 2e13],
];

/// Forgets the checkpoints taken by the board's time that `THINNED` says
/// are closer to their neighbours than they need be.
function thin() {
  const entries = checkpoints.entries;
  const latest = entries.at(-1).board;
  if (typeof latest !== "number") return;
  for (let i = 1; i < entries.length - 1; i++) {
    const entry = entries[i];
    if (!entry.byBoard) continue;
    const apart = THINNED.find(([within]) => latest - entry.board < within)[1];
    if ((entries[i + 1].board ?? Infinity) - (entries[i - 1].board ?? -Infinity) <= apart && checkpoints.drop(i)) i--;
  }
}

/// A checkpoint, with where the container's board stood at it, in
/// picoseconds of the board's time — null with no board — which is what a
/// seek in the board's time picks its checkpoint by.
function keep(at, container) {
  const index = checkpoints.add(at, container);
  let board = null;
  try {
    board = container.value("board").time_ps ?? null;
  } catch {
    // No board.
  }
  checkpoints.entries[index].board = board;
  return index;
}

function checkpoint(byBoard = false) {
  const index = keep(frontier, live);
  checkpoints.entries[index].byBoard = byBoard;
  ranSinceCheckpoint = 0;
  if (byBoard) thin();
}

/// Live: one slice of the frontier, at most `by` instructions; answers the
/// turn's kind.
let idleFor = 0; // ms the last idle slice said the container can be left
/// Live: the board's screen, sent to the page as soon as a slice has shown
/// it a new frame, rather than asked for: an ask waits behind whatever the
/// board is doing, and frames that come in bunches stutter. At most every
/// `DISPLAY_MS`; a board without a panel is asked only until it says so.
const DISPLAY_MS = 8;
let displayed = 0; // the count of the frame last sent
let displayAt = 0;
let hasDisplay = true;
function sendDisplay() {
  const now = performance.now();
  if (!hasDisplay || now - displayAt < DISPLAY_MS) return;
  displayAt = now;
  let value;
  try {
    value = live.value(`board/display/${displayed}`);
  } catch {
    hasDisplay = false;
    return;
  }
  if (value?.off && value.frame === 0) return;
  if (value?.pixels === undefined) return;
  displayed = value.frame;
  postMessage({ type: "display", value });
}

function slice(by) {
  const began = performance.now();
  const turn = live.step(frontier + by);
  idleFor = turn.wait ?? 0;
  const retired = live.value("statistics").retired;
  const took = performance.now() - began;
  ran += took;
  ranSinceCheckpoint += took;
  frontier = retired;
  if (turn.kind === KIND.FINISHED) {
    finished = turn.status;
    checkpoint();
  } else if (checkpointDue()) checkpoint();
  else if (boardCheckpointDue()) checkpoint(true);
  sendDisplay();
  return turn.kind;
}

/// What the page is told as the frontier moves.
function progress(extra = {}) {
  const { timeline, trace, exchanges, hubris } = growth(live);
  const last = checkpoints.entries.at(-1);
  postMessage({
    type: "progress",
    frontier,
    finished,
    timeline,
    trace,
    exchanges,
    hubris,
    stdout: console_(live, "stdout"),
    listening: edge ? [...edge.listening] : [],
    checkpoints: checkpoints.length,
    held: checkpoints.held,
    // What checkpoints cost and buy: the time taken against the time run,
    // and how far a seek may have to re-execute, in instructions and at
    // the rate the frontier has been running.
    checkpointing: {
      every: checkpointEvery,
      budget,
      spent: checkpoints.spent,
      ran,
      lastCost: checkpoints.lastCost ?? 0,
      gap: frontier - last.at,
      widest: checkpoints.widest(frontier),
      rate: ran ? (frontier - origin) / ran : 0,
    },
    ...extra,
  });
}

/// Live: runs the frontier on by `by` instructions, checkpointing on the
/// way, and reports what happened. Returns early on an idle container: the
/// page may have a request to deliver, and spinning on a parked machine
/// would only burn the core.
async function advance(by) {
  if (!live || finished !== null) return;
  const target = frontier + by;
  let idles = 0;
  while (finished === null && frontier < target) {
    if (slice(target - frontier) !== KIND.IDLE) break;
    if (++idles > 20) break;
    await idle();
  }
  progress({ idle: idles > 20, reply: true });
}

// A turn of the event loop and no more: setTimeout clamps to 4 ms once
// nested, and a message to oneself does not. But Chrome runs a worker's
// messages to itself ahead of the page's to it, so a worker that only ever
// yields that way, as one running a busy board does, can leave the page's
// reads and seeks unanswered for seconds: the screen stalls and then jumps.
// So at least every `FAIR_MS`, a timer instead, which lets them in.
const channel = new MessageChannel();
const yielded = [];
channel.port1.onmessage = () => yielded.shift()?.();
const FAIR_MS = 16;
let fairAt = 0;
const yieldNow = () => {
  const now = performance.now();
  if (now - fairAt >= FAIR_MS) {
    fairAt = now;
    return sleep(0);
  }
  return new Promise((resolve) => (yielded.push(resolve), channel.port2.postMessage(null)));
};

const IDLE_MAX_MS = 50; // the longest an idle container is left, whatever it says

/// Waits as long as an idle container said it could be left — until its
/// next deadline — or until something arrives for it: a message from the
/// page, or a USB transfer completing, either of which calls `wake`. A
/// timer in a busy worker is clamped to about 4 ms, which is longer than
/// most of the deadlines here (a millisecond is common: a USB read timing
/// out), so a shorter wait yields the event loop until the deadline rather
/// than sleep past it.
let waking = null;
function wake() {
  waking?.();
}
async function idle() {
  const until = performance.now() + Math.min(idleFor, IDLE_MAX_MS);
  let woken = false;
  const woke = new Promise((resolve) => (waking = () => ((woken = true), resolve())));
  if (until - performance.now() >= 4) await Promise.race([sleep(until - performance.now() - 1), woke]);
  while (!woken && performance.now() < until) await yieldNow();
  // And a turn of the event loop however short the wait: a board running
  // behind says to wake at once, again and again, and a loop that never
  // yields leaves the page's messages unread for as long as it runs.
  if (!woken) await yieldNow();
  waking = null;
}

const SLICE_MS = 8; // how long one slice should run, between messages
const REPORT_MS = 50; // how often the page hears of the frontier

/// Live, playing: runs the frontier until paused or finished, a slice at a
/// time, sized to take about `SLICE_MS`, so a keystroke or a seek waits at
/// most that long. Reports at most every `REPORT_MS`, and never waits for
/// the page to draw what it reported.
let runs = 0; // which run() is the current one: a pause and a play can overlap the last
async function run() {
  if (running || !live) return;
  running = true;
  const mine = ++runs;
  let size = 1000000;
  let reported = 0;
  try {
    while (running && runs === mine && finished === null) {
      const began = performance.now();
      const from = frontier;
      const kind = slice(size);
      const took = performance.now() - began;
      if (kind === KIND.IDLE) {
        if (performance.now() - reported >= REPORT_MS) (progress({ idle: true }), (reported = performance.now()));
        await idle();
        continue;
      }
      // A slice that ended early, idle or not, says nothing about the rate.
      if (took > 0.5 && frontier - from >= size) size = Math.max(100000, Math.min(4 * size, 20000000, Math.round((size * SLICE_MS) / took)));
      if (performance.now() - reported >= REPORT_MS) (progress(), (reported = performance.now()));
      await yieldNow();
    }
  } finally {
    if (runs === mine) {
      running = false;
      progress({ stopped: true });
    }
  }
}

/// One read of the container in view: `{ pattern, value }` or
/// `{ pattern, error }`, never a throw — a refusal is something to show.
function read(container, pattern, path) {
  try {
    return { pattern, value: container.value(path) };
  } catch (why) {
    return { pattern, error: String(why) };
  }
}

/// A pattern of the meta lens with its parameters filled from `fill`, or
/// null when one of them has no value here.
function instantiate(pattern, fill) {
  let missing = false;
  const path = pattern.replace(/\{(\w+)\}/g, (_, name) => {
    if (fill[name] === undefined || fill[name] === null) missing = true;
    return String(fill[name] ?? "");
  });
  return missing ? null : path.replace(/\/+$/, "");
}

/// Stands a container at `at`: the frontier itself when asked for the
/// frontier, or for null, which is wherever the frontier is now; otherwise a restored checkpoint run exactly to the instant.
async function stand(at, board = null) {
  if (board) return standBoard(board);
  if (live && at === null) at = frontier;
  if (at < origin) at = origin;
  if (live && at >= frontier) {
    viewer = live;
    return 0;
  }
  const began = performance.now();
  const index = checkpoints.before(at);
  viewer = await Container.fromSnapshot(module, checkpoints.snapshot(index));
  if (at > checkpoints.at(index)) viewer.stopAt(at);
  return performance.now() - began;
}

/// The container standing where its board stands at `time` picoseconds of
/// the board's own time: from the last checkpoint before the board got
/// there, run on until it does. A count of the container's instructions
/// doesn't name such an instant — an idle container retires none while its
/// board runs on — so it is a seek of its own.
///
/// With `follow`, where values go is followed on the way (`board/provenance`),
/// or with `follow: { from }`, every value, from that instant of the board's;
/// from a checkpoint far enough back — `FOLLOW_BACK` of the board's time —
/// that what arrived shortly before the instant is caught too, and never
/// one from before there was a board to follow. With `watch`, writes are
/// watched on the way likewise (`board/watch`: a range's every write, or
/// who writes what), from `back` picoseconds before the instant.
const FOLLOW_BACK = 2e12;
async function standBoard(board) {
  const began = performance.now();
  viewer = await boardAt(board, reporter("stand"));
  return performance.now() - began;
}

/// A container restored and run to where its board stands at `time`: see
/// `standBoard`. Its own, so that a seek made while it runs on cannot
/// change which one it is.
async function boardAt({ time, follow = false, watch = null, back = FOLLOW_BACK }, report = null) {
  // `follow: { from }`: everything followed, from that instant; `follow:
  // { forward: { from, address, length } }`: those bytes, forward from it.
  const forward = typeof follow === "object" && follow?.forward ? follow.forward : null;
  const everything = typeof follow === "object" && follow !== null && !forward ? Math.max(0, Math.round(follow.from)) : null;
  const from = forward ? Math.max(0, Math.round(forward.from)) : everything ?? (follow || watch ? time - (watch ? back : FOLLOW_BACK) : time);
  const boarded = (entry) => entry.board !== null && entry.board !== undefined;
  // The last checkpoint before `from`; following or watching, failing
  // that, the first with a board at all.
  let index = checkpoints.entries.findLastIndex((entry) => boarded(entry) && entry.board < from);
  if (index < 0) index = follow || watch ? Math.max(0, checkpoints.entries.findIndex((entry) => boarded(entry) && entry.board < time)) : 0;
  const container = await Container.fromSnapshot(module, checkpoints.snapshot(index));
  const following = forward ? `forward ${from} ${forward.address} ${forward.length}` : everything !== null ? `everything ${everything}` : follow && "on";
  for (const [path, on] of [["board/provenance", following], ["board/watch", watch]]) {
    if (!on) continue;
    try {
      container.put(path, on);
    } catch {
      // No board at that checkpoint: nothing to follow or watch.
    }
  }
  const start = checkpoints.entries[index]?.board ?? 0;
  const watching = forward || everything !== null || follow || watch ? from : null;
  container.stopAtBoard({ time }, live ? frontier : -1, report && ((at) => report({ start, from: watching, to: time, board: at.board })));
  return container;
}

/// What tells the page how far a run toward an instant has got, at most
/// every `PROGRESS_MS`: `{ type: "rerun", id, start, from, to, board,
/// elapsed }` — the board's time at the checkpoint it began from, where
/// following or watching begins (null for neither), the instant, where it
/// has got to, and the milliseconds it has taken.
const PROGRESS_MS = 150;
function reporter(id) {
  const began = performance.now();
  let last = 0;
  return (at) => {
    const now = performance.now();
    if (now - last < PROGRESS_MS) return;
    last = now;
    postMessage({ type: "rerun", id, ...at, elapsed: now - began });
  };
}

/// One path read at an instant of the board's, from a container stood
/// there with `board`'s options on the way: what a variable's history and
/// an audit are, read once the re-execution that made them is done. The
/// panels' own instant is left as it was.
async function query(id, board, path) {
  const began = performance.now();
  const container = await boardAt(board, reporter(id));
  const answer = read(container, path, path);
  postMessage({ type: "value", id, path, restored: performance.now() - began, ...answer });
}

/// The machine at `at`, as every path of its store the instant can fill.
/// `context` says which process and thread the page is looking at (the
/// running ones when it says nothing) and where it is browsing the files.
async function seek(at, context = {}, board = null) {
  const began = performance.now();
  const restored = await stand(at, board);
  const values = {};
  const statistics = read(viewer, "statistics", "statistics");
  const processes = read(viewer, "processes", "processes");
  values.statistics = statistics;
  values.processes = processes;
  const all = processes.value?.processes ?? [];
  const running = statistics.value?.current;
  const current = all.find((p) => p.pid === context.pid) ?? all.find((p) => p.pid === running) ?? all[0];
  const thread = current?.threads.find((t) => t.tid === context.tid) ?? current?.threads.find((t) => t.state === "runnable") ?? current?.threads[0];
  const fill = { pid: current?.pid, tid: thread?.tid, path: (context.path ?? "").replace(/^\/+/, "") };
  for (const [pattern, lens] of Object.entries(meta?.paths ?? {})) {
    if (!lens.readable || pattern === "meta" || pattern.startsWith("meta/") || pattern.includes("{address}")) continue;
    const path = instantiate(pattern, fill);
    if (path === null || values[path]) continue;
    values[path] = read(viewer, pattern, path);
  }
  // The memory under the stack pointer, once the registers say where it is.
  const registers = fill.tid !== undefined ? values[`processes/${fill.pid}/threads/${fill.tid}/registers`]?.value : null;
  if (registers?.rsp) {
    const pattern = "processes/{pid}/memory/{address}/{length}";
    const path = instantiate(pattern, { ...fill, address: registers.rsp, length: 256 });
    values[path] = read(viewer, pattern, path);
  }
  postMessage({
    type: "state",
    at: statistics.value?.retired ?? at,
    restored,
    // What the seek cost, restoring aside: what the page paces its
    // following of a live run by, so that reading every panel does not
    // take the time a busy board needs.
    took: performance.now() - began - restored,
    pid: current?.pid ?? null,
    tid: thread?.tid ?? null,
    values,
    stdout: console_(viewer, "stdout"),
    stderr: console_(viewer, "stderr"),
    log: text(viewer.readback(["iso", "log", "error"]) ?? new Uint8Array()),
  });
}

/// One path, read at the instant in view.
function readOne(id, path) {
  const answer = viewer ? read(viewer, path, path) : { pattern: path, error: "nothing is loaded" };
  postMessage({ type: "value", id, path, ...answer });
}

/// Live: a request through the edge. The response arrives once the guest
/// has run far enough to answer, which `advance` drives.
function request(id, port, body) {
  if (!edge) return postMessage({ type: "response", id, error: "not a live run" });
  const sent = frontier;
  edge
    .request(port, new TextEncoder().encode(body))
    .then((response) => {
      // The page places the answer on its timeline, so the timeline up to
      // it goes first: a running frontier would otherwise report it later.
      progress();
      postMessage({ type: "response", id, response: text(response), sent, answered: frontier });
    })
    .catch((why) => postMessage({ type: "response", id, error: String(why) }));
}

/// Live: a connection that stays open, as a terminal's does. What the guest
/// writes is posted as the frontier runs into it.
const streams = new Map();
function openStream(id, port) {
  if (!edge) return postMessage({ type: "stream", id, ended: true, error: "not a live run" });
  const handle = edge.open(port, {
    received: (data) => postMessage({ type: "stream", id, data: text(data) }),
    ended: () => {
      streams.delete(id);
      postMessage({ type: "stream", id, ended: true });
    },
  });
  streams.set(id, handle);
}

onmessage = async (event) => {
  const message = event.data;
  // Anything the page sends may be what a parked container waits for.
  wake();
  try {
    if (message.type === "load") await load(message);
    else if (message.type === "advance") await advance(message.by);
    else if (message.type === "run") run().catch((why) => postMessage({ type: "error", message: String(why?.stack ?? why) }));
    else if (message.type === "pause") running = false;
    else if (message.type === "checkpointing") {
      if (message.every !== undefined) checkpointEvery = message.every;
      if (message.budget !== undefined) budget = message.budget;
    }
    else if (message.type === "seek") await seek(message.at, message.context, message.board);
    else if (message.type === "read") readOne(message.id, message.path);
    else if (message.type === "query") await query(message.id, message.board, message.path);
    else if (message.type === "request") request(message.id, message.port, message.request);
    else if (message.type === "open") openStream(message.id, message.port);
    // Text a line at a time, or bytes: a file dropped on the terminal.
    else if (message.type === "write") {
      const bytes = typeof message.data === "string" ? new TextEncoder().encode(message.data) : new Uint8Array(message.data);
      streams.get(message.id)?.send(bytes);
    }
    // The page has been granted a USB device: attach it, and the next
    // program to start finds it.
    else if (message.type === "usb") await usb?.refresh();
  } catch (why) {
    postMessage({ type: "error", message: String(why?.stack ?? why) });
  }
};
