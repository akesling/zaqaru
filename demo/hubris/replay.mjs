// Checks that the Hubris bench's board replays: a live run, recorded as the
// page records one, is checkpointed on the way, and the board is read after
// every slice of it. Then each checkpoint is restored and run on against
// the recording, the board read after every slice again, and wherever the
// two have read the board at the same count of its instructions, every
// register, pin, part, the simulated time and a digest of all its memory
// must match. That is what
// anything that rewinds the board to ask a question after the fact stands
// on.
//
// The board is compared by its own instruction count, not the container's:
// an idle container retires no instructions while the board, moved on at
// each of its idle turns against a recorded clock reading, runs on — so one
// count of the container's instructions is many instants of the board's,
// and a seek to it (`stopAt`) stands at the first of them. The check also
// shows that.
//
//   node demo/hubris/replay.mjs <hubris.wasm> [line...]
//
// The lines, typed one after another into a session on port 80, default to
// a Humility read of the TMP117 on I2C2; a checkpoint is taken at the
// prompt, and again partway through each line, mid-transfer. Exits 0 when
// every restored board matches.

import { readFileSync } from "node:fs";
import { Checkpoints } from "../../web/checkpoints.js";
import { Container, Edge, KIND, standardMounts, text } from "../../web/zaqaru.js";

const [modulePath, ...asked] = process.argv.slice(2);
if (!modulePath) {
  console.error("usage: node demo/hubris/replay.mjs <hubris.wasm> [line...]");
  process.exit(2);
}
const lines = asked.length ? asked : ["humility i2c -c 2 -p F -d 0x48 -r 0 -n 2"];
const PROMPT = "\n$ ";
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const module = await WebAssembly.compile(readFileSync(modulePath));
const edge = new Edge([80]);
const mounts = standardMounts({ seed: null, edge });
mounts.record();
const live = await Container.instantiate(module, mounts);
live.step(0);

const checkpoints = new Checkpoints();
// Where the board stood at each checkpoint, in picoseconds of its time:
// what a seek in the board's time picks its checkpoint by.
const boardAt = [];
const checkpoint = (at) => {
  checkpoints.add(at, live);
  let time = null;
  try {
    time = live.value("board/digest/memory").time_ps;
  } catch {
    // No board yet.
  }
  boardAt.push(time);
};
checkpoint(0);
let retired = 0;
let screen = "";
const session = edge.open(80, { received: (data) => (screen += text(data)), ended: () => {} });

// The board as read after each slice, by the count of its instructions.
const seen = new Map();
// With a digest of all its memory — each RAM and the flash — so that a
// difference nothing the board's JSON shows would still be caught.
const sample = (container, into) => {
  const board = container.value("board");
  const memory = container.value("board/digest/memory");
  into.set(board.instructions, JSON.stringify({ ...board, memory }));
  return board;
};

// As the page runs a live container: in slices, waiting as long as an idle
// one says it can be left. `midway`, once, after that many slices.
async function runUntil(done, limit, midway = null) {
  const from = performance.now();
  let slices = 0;
  while (!done() && performance.now() - from < limit) {
    const turn = live.step(retired + 4000000);
    retired = live.value("statistics").retired;
    sample(live, seen);
    if (turn.kind === KIND.FINISHED) throw `the container exited ${turn.status}`;
    if (++slices === midway?.after) midway.then();
    if (turn.kind === KIND.IDLE) await (turn.wait >= 1 ? sleep(Math.min(turn.wait, 50)) : new Promise(setImmediate));
  }
  return done();
}

if (!(await runUntil(() => screen.endsWith("$ "), 60000))) throw "no prompt";
checkpoint(retired);
console.error(`replay: checkpoint at the prompt, ${retired.toLocaleString()} instructions`);
for (const line of lines) {
  const mark = screen.length;
  session.send(new TextEncoder().encode(`${line}\n`));
  const midway = {
    after: 6,
    then: () => {
      checkpoint(retired);
      console.error(`replay: checkpoint mid-command, ${retired.toLocaleString()} instructions`);
    },
  };
  if (!(await runUntil(() => screen.length > mark && screen.endsWith(PROMPT), 600000, midway))) throw `no prompt after ${line}`;
  console.log(`$ ${line}\n${screen.slice(mark, -PROMPT.length)}`);
}
// A little more, with the board running on its own.
await runUntil(() => false, 2000);
const at = retired;
const want = live.value("board");
console.error(`replay: the live run stops at ${at.toLocaleString()} instructions, the board at ${want.simulated_ms} ms simulated`);

// Every field that differs, by its path in the JSON.
function differences(a, b, path = "", out = []) {
  if (typeof a !== typeof b || a === null || b === null || typeof a !== "object") {
    if (JSON.stringify(a) !== JSON.stringify(b)) out.push(`${path || "."}: live ${JSON.stringify(a)?.slice(0, 80)}, replayed ${JSON.stringify(b)?.slice(0, 80)}`);
    return out;
  }
  for (const key of new Set([...Object.keys(a), ...Object.keys(b)])) differences(a[key], b[key], `${path}/${key}`, out);
  return out;
}

let ok = true;
for (let index = 0; index < checkpoints.length; index++) {
  const from = checkpoints.at(index);
  const began = performance.now();
  const viewer = await Container.fromSnapshot(module, checkpoints.snapshot(index));
  const replayed = new Map();
  let board = sample(viewer, replayed);
  let position = from;
  while (board.instructions < want.instructions) {
    const turn = viewer.step(position + 4000000);
    position = viewer.value("statistics").retired;
    board = sample(viewer, replayed);
    if (turn.kind === KIND.FINISHED) break;
  }
  let same = 0;
  const differ = [];
  for (const [count, json] of replayed) {
    if (!seen.has(count)) continue;
    if (seen.get(count) === json) same++;
    else differ.push(count);
  }
  const seconds = ((performance.now() - began) / 1000).toFixed(1);
  console.error(
    `replay: from checkpoint ${index} (${from.toLocaleString()}): ${replayed.size} boards read, ${same + differ.length} at instants the live run read too: ${same} the same, ${differ.length} different; ${seconds} s`,
  );
  if (differ.length) {
    const count = differ[0];
    for (const d of differences(JSON.parse(seen.get(count)), JSON.parse(replayed.get(count))).slice(0, 12)) console.error(`    at ${count}: ${d}`);
  }
  ok &&= differ.length === 0 && same > 0;
}

// And what a seek by the container's count gives: the first of the board's
// instants at that count, which is behind the live run's by whatever the
// board ran while the container idled there. Said, not checked.
{
  const viewer = await Container.fromSnapshot(module, checkpoints.snapshot(checkpoints.length - 1));
  viewer.stopAt(at);
  console.error(
    `replay: a seek to ${at.toLocaleString()} container instructions stands the board at ${viewer.value("board").simulated_ms} ms; the live run read it at ${want.simulated_ms} ms`,
  );
}

// A seek in the board's own time does name the instant: for a spread of
// what the live run read — the last few while the container idled — the
// board stood at that time, from the checkpoint before it, must be the
// board the live run read, to the last byte of its memory.
const samples = [...seen.values()].map((json) => JSON.parse(json)).sort((a, b) => a.memory.time_ps - b.memory.time_ps);
const picked = [...Array(9).keys()].map((n) => samples[Math.floor((n * (samples.length - 4)) / 8)]).concat(samples.slice(-3));
let landed = 0;
for (const want of picked) {
  const time = want.memory.time_ps;
  let index = 0;
  boardAt.forEach((t, n) => {
    if (t !== null && t < time) index = n;
  });
  const began = performance.now();
  const viewer = await Container.fromSnapshot(module, checkpoints.snapshot(index));
  viewer.stopAtBoard({ time });
  const got = { ...viewer.value("board"), memory: viewer.value("board/digest/memory") };
  const diff = differences(want, got);
  const seconds = ((performance.now() - began) / 1000).toFixed(1);
  console.error(`replay: the board stood at ${(time / 1e9).toFixed(6)} ms from checkpoint ${index}: ${diff.length ? `${diff.length} differences` : "the board the live run read"}, ${seconds} s`);
  for (const d of diff.slice(0, 8)) console.error(`    ${d}`);
  if (!diff.length) landed++;
  ok &&= diff.length === 0;
}
// And by its instruction count: just after that instruction, exactly.
{
  const want = picked[4];
  const target = want.instructions - 12345;
  const viewer = await Container.fromSnapshot(module, checkpoints.snapshot(0));
  viewer.stopAtBoard({ instructions: target });
  const got = viewer.value("board");
  const exact = got.instructions === target && got.simulated_ms <= want.simulated_ms;
  console.error(`replay: the board stood at instruction ${target.toLocaleString()}: ${exact ? "exactly there" : `at ${got.instructions}`}, ${got.simulated_ms} ms`);
  ok &&= exact;
}
console.error(`replay: ${landed} of ${picked.length} board instants landed on exactly`);
process.exit(ok ? 0 : 1);
