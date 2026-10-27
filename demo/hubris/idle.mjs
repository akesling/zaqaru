// Checks that the Hubris bench survives being left alone: once its
// processes are all parked — the door waiting for a connection — the
// container makes no syscalls, but its kernel still wakes to move the
// board on, reading the host's clock each time. What the host places in
// guest memory for those reads must be dropped between turns, or the
// transfer arena fills a reading at a time and the module traps (it did,
// after about 340,000 idle turns: half an hour of a bench left alone).
//
//   node demo/hubris/idle.mjs <hubris.wasm> [turns]
//
// Runs that many idle turns (default 1,000,000) as fast as it can, and
// exits 0 if the container is still answering after them.

import { readFileSync } from "node:fs";
import { Container, Edge, KIND, standardMounts, text } from "../../web/zaqaru.js";

const [modulePath, asked] = process.argv.slice(2);
if (!modulePath) {
  console.error("usage: node demo/hubris/idle.mjs <hubris.wasm> [turns]");
  process.exit(2);
}
const turns = Number(asked ?? 1000000);
const module = await WebAssembly.compile(readFileSync(modulePath));
const edge = new Edge([80]);
const container = await Container.instantiate(module, standardMounts({ seed: null, edge }));
container.step(0);
let retired = 0;
let screen = "";
edge.open(80, { received: (data) => (screen += text(data)), ended: () => {} });
while (!screen.endsWith("$ ")) {
  container.step(retired + 4000000);
  retired = container.value("statistics").retired;
  await new Promise(setImmediate);
}
const began = performance.now();
let idle = 0;
for (let n = 0; n < turns; n++) {
  try {
    if (container.step(retired + 4000000).kind === KIND.IDLE) idle++;
  } catch (why) {
    console.error(`idle: the module trapped after ${n.toLocaleString()} turns (${idle.toLocaleString()} idle): ${why}`);
    process.exit(1);
  }
}
const board = container.value("board");
console.error(
  `idle: ${turns.toLocaleString()} turns, ${idle.toLocaleString()} of them idle, in ${((performance.now() - began) / 1000).toFixed(0)} s; the board ${board.state} at ${(board.time_ps / 1e12).toFixed(3)} s`,
);
