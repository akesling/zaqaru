// Checks `bench.js` under Node: node --test web/bench.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { channelOf, drawBoard, ethLine, i2cLine, i2cTransactions, ipv6, pinState, probeBody, probeKind, spiTransactions, usartText, waveform } from "./bench.js";

/// A board as the store has one: the bench's Nucleo, its LED on PB0 on.
const board = {
  name: "nucleo-h753zi",
  chip: "stm32h753",
  core: "Cortex-M7",
  clocks: { core: 400000000 },
  simulated_ms: 3000,
  probe: "127.0.0.1:3333",
  serial: "",
  leds: [{ pin: "PB0", color: "green", state: "on" }, { pin: "PB14", color: "red", state: null }],
  gpio: [
    { port: "A", idr: 0, odr: 0, moder: 0xffffffff },
    { port: "B", idr: 1, odr: 1, moder: 0xfffffff5 },
    { port: "C", idr: 1 << 13, odr: 0, moder: 0xf3ffffff },
  ],
  parts: [
    { bus: "I2C2", at: "0x48", kind: "TMP117", line: null, values: { temperature: "25.0000 °C" } },
    { bus: "SPI3", at: "PA15", kind: "RoT", line: "PD0", values: { state: "idle" } },
  ],
};

test("a pin's state is its level and its mode", () => {
  assert.deepEqual(pinState(board, "PB0"), { high: 1, mode: "output", out: 1 });
  assert.equal(pinState(board, "PC13").mode, "input");
  assert.equal(pinState(board, "PA3").mode, "analog");
});

test("the board is drawn with everything probeable marked", () => {
  const svg = drawBoard(board, new Set(["PB0"]));
  assert.match(svg, /^<svg/);
  assert.match(svg, /NUCLEO-H753ZI/);
  for (const probe of ["PB0", "PC13", "I2C2", "SPI3", "ETH", "USART1"]) assert.match(svg, new RegExp(`data-probe="${probe}"`), probe);
  assert.match(svg, /data-part="I2C2 0x48"/);
  assert.match(svg, /stroke="#ff5d5d"/, "the probed pin is outlined");
  assert.equal(svg.match(/<svg/g).length, svg.match(/<\/svg>/g).length);
});

test("probes are of a kind, and read a channel", () => {
  assert.deepEqual(["PB0", "I2C2", "SPI3", "USART1", "ETH", "I2C2 0x48"].map(probeKind), ["pin", "i2c", "spi", "usart", "eth", "part"]);
  assert.equal(channelOf("I2C2 0x48"), null);
});

test("an I2C register read decodes as the bus carried it", () => {
  // As the bench's trace served Humility's read of the TMP117.
  const records = [
    { t: 1, ev: "start", addr: 72, read: false, ack: true },
    { t: 2, ev: "byte", b: 0 },
    { t: 3, ev: "start", addr: 72, read: true, ack: true },
    { t: 4, ev: "byte", b: 12 },
    { t: 5, ev: "byte", b: 128 },
    { t: 6, ev: "stop" },
    { t: 7, ev: "start", addr: 80, read: false, ack: false },
    { t: 8, ev: "stop" },
  ];
  const transactions = i2cTransactions(records);
  assert.equal(transactions.length, 2);
  assert.equal(i2cLine(transactions[0]), "0x48 W 00 · 0x48 R 0c 80");
  assert.equal(i2cLine(transactions[1]), "0x50 W NACK");
});

test("SPI frames group by chip-select", () => {
  const records = [
    { t: 1, ev: "select", cs: "PD14" },
    { t: 2, ev: "frame", mosi: 0x9f, miso: 0xff },
    { t: 3, ev: "frame", mosi: 0, miso: 0xef },
    { t: 4, ev: "deselect", cs: "PD14" },
  ];
  assert.deepEqual(spiTransactions(records), [{ t: 1, cs: "PD14", mosi: [0x9f, 0], miso: [0xff, 0xef], end: 4 }]);
});

test("USART bytes read as text each way", () => {
  const records = [..."Ping!\r\n"].map((c, i) => ({ t: i, rx: false, b: c.charCodeAt(0) })).concat([{ t: 9, rx: true, b: 1 }]);
  assert.equal(usartText(records, false), "Ping!\n");
  assert.equal(usartText(records, true), "\\x01");
});

test("an Ethernet frame's headers summarize", () => {
  // udpbroadcast's datagram, as the trace kept its head.
  const head = "3333000000010e1d35d5200186dd6000000000351140fe800000000000000c1d35fffed52001ff02000000000000000000000000000103e522b80035012001";
  assert.equal(ethLine({ head }), "0e:1d:35:d5:20:01 → 33:33:00:00:00:01 · IPv6 fe80::c1d:35ff:fed5:2001 → ff02::1 UDP 997 → 8888");
  const tagged = "3333000000010e1d35d520018100030186dd";
  assert.match(ethLine({ head: tagged }), /VLAN 0x301/);
  assert.equal(ipv6([0, 0, 0, 0, 0, 0, 0, 1]), "::1");
});

test("a waveform steps at each edge", () => {
  const svg = waveform([{ t: 500, high: true }, { t: 1500, high: false }], 2000, 2000, 0);
  assert.match(svg, /M 0 36 H 170.0 V 8 H 510.0 V 36 H 680/);
});

test("each probe shows something, edges or not", () => {
  for (const name of ["PB0", "I2C2", "SPI3", "USART1", "ETH", "I2C2 0x48", "SPI9 PZ1"]) {
    assert.equal(typeof probeBody(name, [], board, 1e9), "string", name);
  }
  assert.match(probeBody("I2C2 0x48", [], board, 1e9), /25\.0000 °C/);
});

test("a Hubris system call reads as what it asks", async () => {
  const { syscallLine } = await import("./bench.js");
  const names = (n) => ["jefe", "sys", "i2c_driver"][n] ?? `task ${n}`;
  assert.equal(syscallLine({ r11: 0, r4: (2 << 16) | 1, r5: 0 }, names).text, "send → i2c_driver op 1");
  assert.equal(syscallLine({ r11: 0, r4: (0xffff << 16) | 2, r5: 0 }, names).text, "kipc reinit_task");
  assert.equal(syscallLine({ r11: 0, r4: (0xffff << 16) | 2, r5: 0 }, names).kind, "kernel");
  assert.equal(syscallLine({ r11: 2, r4: (3 << 10) | 1, r5: 0 }, names).text, "reply → sys");
  assert.equal(syscallLine({ r11: 1, r4: 0x2400_0000, r5: 24 }, names).text, "recv");
  assert.equal(syscallLine({ r11: 8, r4: 0, r5: 0 }, names).kind, "fault");
});

test("the timeline's task lane starts the window with the task that was running", async () => {
  const { timelineLanes } = await import("./bench.js");
  const records = new Map([
    ["TASK", [{ t: 100, task: 19 }, { t: 1500, task: 9 }, { t: 1700, task: 19 }]],
    ["SYS", [{ t: 1510, r11: 1, r4: 0, r5: 0 }]],
    ["EXC", [{ t: 1000, exc: 15, name: "SysTick" }, { t: 1600, exc: 4, name: "MemManage" }]],
  ]);
  const { lanes } = timelineLanes(records, { tasks: true, syscalls: true, interrupts: true }, 1000, 2000, (n) => `t${n}`);
  assert.deepEqual(lanes[0].spans.map((s) => [s.from, s.to, s.label]), [[1000, 1500, "t19"], [1500, 1700, "t9"], [1700, 2000, "t19"]]);
  assert.deepEqual(lanes[1].marks.map((m) => m.text), ["recv"]);
  assert.deepEqual(lanes[2].marks.map((m) => m.kind), ["tick", "fault"]);
});
