// Plays the Doom bench under Node, as the page does in a browser, and says
// how fast it goes: the board left to run against the container's clock
// for a while, a new game started from the menu by keys typed at the door,
// and then the frames the panel was shown, a second of this host's time,
// and how far behind real time the board fell.
//
//   node demo/doom/check.mjs <doom.wasm> [seconds] [screen.ppm] [max]
//
// Runs that many seconds (default 30) of this host's time, writes the last
// screen to screen.ppm if asked, and exits 0 if the panel was shown frames
// of the game itself and the task never fell over. With `max`, the board
// runs as fast as it can (the door's `speed max`) rather than in real time,
// which says how fast a host could play it.

import { readFileSync, writeFileSync } from "node:fs";
import { Container, Edge, standardMounts, text } from "../../web/zaqaru.js";

const [modulePath, asked, shot, flat] = process.argv.slice(2);
if (!modulePath) {
  console.error("usage: node demo/doom/check.mjs <doom.wasm> [seconds] [screen.ppm] [max]");
  process.exit(2);
}
const seconds = Number(asked ?? 30);
const module = await WebAssembly.compile(readFileSync(modulePath));
const edge = new Edge([80]);
const container = await Container.instantiate(module, standardMounts({ seed: null, edge }));
container.step(0);
let retired = 0;
let screen = "";
const door = edge.open(80, { received: (data) => (screen += text(data)), ended: () => {} });
const run = () => {
  container.step(retired + 4000000);
  retired = container.value("statistics").retired;
};
while (!screen.endsWith("$ ")) {
  run();
  await new Promise(setImmediate);
}

// Into a game from the title: any key for the menu, then New Game, the
// episode and the skill, each Enter, its scan code down (0x1c) and up.
const enter = "keys 1c 9c\n";
const typed = [
  ...(flat === "max" ? [[0, "speed max\n"]] : []),
  [6, enter],
  [7, enter],
  [8, enter],
  [9, enter],
];
const began = performance.now();
const elapsed = () => (performance.now() - began) / 1000;
const first = container.value("board");
let samples = [];
let last = 0;
while (elapsed() < seconds) {
  run();
  while (typed.length && elapsed() >= typed[0][0]) door.send(new TextEncoder().encode(typed.shift()[1]));
  if (elapsed() - last >= 1) {
    last = elapsed();
    const board = container.value("board");
    samples.push({ at: elapsed(), ms: board.simulated_ms, frames: board.display?.frames ?? 0, instructions: board.instructions });
  }
  await new Promise(setImmediate);
}
const board = container.value("board");
const wall = elapsed();
const frames = (board.display?.frames ?? 0) - (first.display?.frames ?? 0);
const simulated = (board.simulated_ms - first.simulated_ms) / 1000;
const instructions = board.instructions - first.instructions;
console.log(
  `${wall.toFixed(1)} s here: the board ran ${simulated.toFixed(1)} s (${((100 * simulated) / wall).toFixed(0)}% of real time), ` +
    `${(instructions / wall / 1e6).toFixed(1)} M instructions a second, and showed ${frames} frames, ${(frames / wall).toFixed(1)} a second`,
);
// The last ten seconds, in the game.
const tail = samples.filter((s) => s.at >= wall - 10);
if (tail.length >= 2) {
  const [a, b] = [tail[0], tail.at(-1)];
  console.log(
    `the last ${(b.at - a.at).toFixed(0)} s: ${((b.frames - a.frames) / (b.at - a.at)).toFixed(1)} frames a second, ` +
      `the board at ${((100 * (b.ms - a.ms)) / 1000 / (b.at - a.at)).toFixed(0)}% of real time, ` +
      `${((b.instructions - a.instructions) / (b.at - a.at) / 1e6).toFixed(1)} M instructions a second`,
  );
}

const display = container.value(`board/display/0`);
if (shot && display.pixels) {
  const pixels = Buffer.from(display.pixels, "base64");
  const palette = Buffer.from(display.palette, "base64");
  const rgb = display.format === "indexed" ? Buffer.concat([...pixels].map((i) => palette.subarray(3 * i, 3 * i + 3))) : pixels;
  writeFileSync(shot, Buffer.concat([Buffer.from(`P6 ${display.width} ${display.height} 255\n`), rgb]));
}
const console_ = board.serial;
const crashed = /Z_Malloc|I_Error|exited/.test(console_) || /Error/.test(console_.split("Auto-scaling factor: 1").at(-1) ?? "");
if (crashed) console.error(`the game fell over:\n${console_.slice(-800)}`);
process.exit(frames > 0 && !crashed ? 0 : 1);
