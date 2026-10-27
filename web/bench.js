// The bench: the board on the container's bench, drawn, with probes on it.
//
// The drawing is made from what the board says of itself (the `board`
// path): which board it is, its chip and clocks, every GPIO pin's level and
// mode, its LEDs, and the parts on its buses. Every pin, bus, serial line,
// the Ethernet jack and each part can be probed: click it, and a probe
// shows what crosses it, from the board's trace (`board/trace/{since}/
// {channels}`), in the board's simulated time — a pin's level as a
// waveform, an I2C or SPI bus's transactions decoded, a USART's bytes as
// text, Ethernet's frames summarized, a part's values as it holds them.
//
// Everything here is a pure function of the board's state and the records
// a probe has gathered, so that it can be checked without a page: see
// `bench.test.mjs`.

/// The boards the bench knows by name: what to call them and what colour
/// their board is. Any other board is drawn plainly, named by its chip.
export const BOARDS = {
  "nucleo-h753zi": { title: "NUCLEO-H753ZI", maker: "STMicroelectronics · Nucleo-144", pcb: "#f3f3ef", ink: "#1f2328", button: "PC13" },
  "nucleo-h743zi2": { title: "NUCLEO-H743ZI2", maker: "STMicroelectronics · Nucleo-144", pcb: "#f3f3ef", ink: "#1f2328", button: "PC13" },
  "stm32g031-nucleo": { title: "NUCLEO-G031K8", maker: "STMicroelectronics · Nucleo-32", pcb: "#f3f3ef", ink: "#1f2328" },
  "stm32g070-nucleo": { title: "NUCLEO-G070RB", maker: "STMicroelectronics · Nucleo-64", pcb: "#f3f3ef", ink: "#1f2328", button: "PC13" },
  "gimletlet-2": { title: "Gimletlet 2", maker: "Oxide Computer Company · SP development board", pcb: "#1e4630", ink: "#e9f1eb" },
  "psc-b": { title: "PSC rev B", maker: "Oxide Computer Company · power shelf controller", pcb: "#17283c", ink: "#e6edf5" },
  "psc-c": { title: "PSC rev C", maker: "Oxide Computer Company · power shelf controller", pcb: "#17283c", ink: "#e6edf5" },
};

export function boardOf(value) {
  return BOARDS[value.name] ?? { title: value.name || value.chip.toUpperCase(), maker: "", pcb: "#2a2f36", ink: "#e6e8eb" };
}

const MODES = ["input", "output", "alternate", "analog"];

/// A pin's state: its level, and its mode from MODER.
export function pinState(value, name) {
  const port = value.gpio?.find((p) => p.port === name[1]);
  const pin = Number(name.slice(2));
  if (!port) return null;
  return { high: (port.idr >> pin) & 1, mode: MODES[(port.moder >>> (2 * pin)) & 3], out: (port.odr >> pin) & 1 };
}

const esc = (text) => String(text).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const hex2 = (b) => b.toString(16).padStart(2, "0");

/// What each pin is to the board, beyond the chip: an LED, the button, a
/// chip-select, a part's interrupt line.
export function pinRoles(value) {
  const roles = new Map();
  const add = (pin, role) => roles.set(pin, roles.has(pin) ? `${roles.get(pin)}; ${role}` : role);
  for (const led of value.leds ?? []) add(led.pin, `${led.color} LED`);
  const board = boardOf(value);
  if (board.button) add(board.button, "the user button");
  for (const part of value.parts ?? []) {
    if (part.bus.startsWith("SPI")) add(part.at, `${part.kind}'s chip-select on ${part.bus}`);
    if (part.line) add(part.line, `${part.kind}'s interrupt line`);
  }
  return roles;
}

/// The board, drawn: an SVG, every probeable thing in it carrying
/// `data-probe` (and a part `data-part`), the probed ones outlined.
export function drawBoard(value, probed = new Set()) {
  const board = boardOf(value);
  const roles = pinRoles(value);
  const W = 760;
  const ports = value.gpio ?? [];
  const cell = 11;
  const gridX = 40;
  const gridY = 132;
  const mcuW = gridX + 16 * cell + 26 - 24;
  const buses = groupParts(value.parts ?? []);
  const rightX = 330;
  // Each bus a block, with its parts below its name.
  let busY = 70;
  const busBlocks = [];
  for (const [bus, parts] of buses) {
    const h = 26 + parts.length * 30;
    busBlocks.push({ bus, parts, y: busY, h });
    busY += h + 12;
  }
  const serial = value.serial !== undefined;
  const connectorCount = (value.chip.startsWith("stm32h7") ? 1 : 0) + (serial ? 1 : 0) + 1;
  const H = Math.max(gridY + ports.length * cell + 76, busY + 4, 70 + connectorCount * 78 + 4, 230);
  const out = [];
  out.push(`<svg class="bench-board" viewBox="0 0 ${W} ${H}" width="100%" xmlns="http://www.w3.org/2000/svg" font-family="ui-monospace, Menlo, monospace">`);
  out.push(`<rect x="2" y="2" width="${W - 4}" height="${H - 4}" rx="14" fill="${board.pcb}" stroke="#0006"/>`);
  // Mounting holes, as a board has.
  for (const [x, y] of [[18, 18], [W - 18, 18], [18, H - 18], [W - 18, H - 18]]) {
    out.push(`<circle cx="${x}" cy="${y}" r="6" fill="#0003" stroke="#c9a54a" stroke-width="2"/>`);
  }
  out.push(`<text x="40" y="34" fill="${board.ink}" font-size="16" font-weight="700">${esc(board.title)}</text>`);
  out.push(`<text x="40" y="52" fill="${board.ink}" opacity=".7" font-size="10">${esc(board.maker)}</text>`);
  // The MCU, its ports as rows of pins.
  const mcuY = 70;
  const mcuH = gridY - mcuY + ports.length * cell + 14;
  out.push(`<rect x="${gridX - 26}" y="${mcuY}" width="${mcuW + 30}" height="${mcuH}" rx="4" fill="#1c1d21" stroke="#555"/>`);
  out.push(`<text x="${gridX - 14}" y="${mcuY + 18}" fill="#ddd" font-size="12" font-weight="700">${esc(value.chip.toUpperCase())}</text>`);
  const mhz = value.clocks ? `${Math.round(value.clocks.core / 1e6)} MHz` : "";
  out.push(`<text x="${gridX - 14}" y="${mcuY + 33}" fill="#999" font-size="10">${esc(value.core ?? "")} · ${esc(mhz)}</text>`);
  out.push(`<text x="${gridX - 14}" y="${mcuY + 47}" fill="#777" font-size="9">click a pin to probe it</text>`);
  for (let p = 0; p < 16; p += 5) {
    out.push(`<text x="${gridX + p * cell + 2}" y="${gridY - 4}" fill="#777" font-size="7">${p}</text>`);
  }
  ports.forEach((port, row) => {
    const y = gridY + row * cell;
    out.push(`<text x="${gridX - 16}" y="${y + 8}" fill="#aaa" font-size="9">P${port.port}</text>`);
    for (let pin = 0; pin < 16; pin++) {
      const name = `P${port.port}${pin}`;
      const high = (port.idr >> pin) & 1;
      const mode = MODES[(port.moder >>> (2 * pin)) & 3];
      const fill =
        mode === "output" ? (high ? "#ffc940" : "#6a5520") : mode === "alternate" ? (high ? "#c39bd3" : "#6c4f7c") : mode === "analog" ? "#33363c" : high ? "#5fc7ff" : "#1f4c66";
      const role = roles.get(name);
      const stroke = probed.has(name) ? "#ff5d5d" : role ? "#fff" : "#0000";
      const title = `${name} — ${mode}, ${high ? "high" : "low"}${role ? ` — ${role}` : ""}`;
      out.push(`<rect data-probe="${name}" x="${gridX + pin * cell}" y="${y}" width="${cell - 2}" height="${cell - 2}" rx="1.5" fill="${fill}" stroke="${stroke}" stroke-width="${probed.has(name) ? 2 : 1}" style="cursor:pointer"><title>${esc(title)}</title></rect>`);
    }
  });
  // A legend for the pins' colours.
  const legend = [["#ffc940", "output high"], ["#6a5520", "output low"], ["#5fc7ff", "input high"], ["#1f4c66", "input low"], ["#c39bd3", "peripheral"], ["#33363c", "analog"]];
  legend.forEach(([fill, label], i) => {
    const x = gridX - 14 + (i % 3) * 82;
    const y = mcuY + mcuH + 16 + Math.floor(i / 3) * 14;
    out.push(`<rect x="${x}" y="${y - 8}" width="9" height="9" rx="1.5" fill="${fill}"/><text x="${x + 13}" y="${y}" fill="${board.ink}" opacity=".75" font-size="9">${label}</text>`);
  });
  // The LEDs, and the button, along the top.
  (value.leds ?? []).forEach((led, i) => {
    const x = 470 + i * 46;
    const on = led.state === "on";
    const color = { green: "#3ddc6a", red: "#ff4d42", yellow: "#ffd23a", blue: "#4aa3ff", white: "#ffffff" }[led.color] ?? "#3ddc6a";
    out.push(`<g data-probe="${led.pin}" style="cursor:pointer"><title>${esc(led.pin)}: ${esc(led.color)} LED, ${esc(led.state ?? "never driven")}</title>`);
    if (on) out.push(`<circle cx="${x}" cy="30" r="14" fill="${color}" opacity=".35"/>`);
    out.push(`<rect x="${x - 7}" y="22" width="14" height="16" rx="3" fill="${on ? color : "#3b3f45"}" stroke="${probed.has(led.pin) ? "#ff5d5d" : "#0007"}" stroke-width="2"/>`);
    out.push(`<text x="${x}" y="52" text-anchor="middle" fill="${board.ink}" font-size="9">${esc(led.pin)}</text></g>`);
  });
  if (board.button) {
    const state = pinState(value, board.button);
    const x = 470 + (value.leds ?? []).length * 46 + 20;
    out.push(`<g data-probe="${board.button}" style="cursor:pointer"><title>the user button, ${board.button}: ${state?.high ? "pressed" : "released"} — the terminal's \`button\` presses it</title>`);
    out.push(`<rect x="${x - 12}" y="20" width="24" height="20" rx="3" fill="#2c2f34" stroke="${probed.has(board.button) ? "#ff5d5d" : "#0007"}" stroke-width="2"/><circle cx="${x}" cy="30" r="6" fill="${state?.high ? "#4aa3ff" : "#4a5059"}"/>`);
    out.push(`<text x="${x}" y="52" text-anchor="middle" fill="${board.ink}" font-size="9">USER</text></g>`);
  }
  // The buses, and the parts on them, joined to the chip.
  const mcuRight = gridX - 26 + mcuW + 30;
  for (const block of busBlocks) {
    const y = block.y;
    const midY = y + 13;
    out.push(`<path d="M ${mcuRight} ${midY} H ${rightX}" stroke="${board.ink}" stroke-opacity=".35" stroke-width="2" fill="none"/>`);
    out.push(`<g data-probe="${block.bus}" style="cursor:pointer"><title>probe ${block.bus}: its transactions, decoded</title>`);
    out.push(`<rect x="${rightX}" y="${y}" width="190" height="${block.h}" rx="5" fill="#0002" stroke="${probed.has(block.bus) ? "#ff5d5d" : board.ink}" stroke-opacity="${probed.has(block.bus) ? 1 : 0.35}" stroke-width="${probed.has(block.bus) ? 2 : 1}"/>`);
    out.push(`<text x="${rightX + 8}" y="${y + 17}" fill="${board.ink}" font-size="11" font-weight="700">${esc(block.bus)}</text></g>`);
    block.parts.forEach((part, i) => {
      const py = y + 24 + i * 30;
      const key = partKey(part);
      out.push(`<g data-part="${esc(key)}" style="cursor:pointer"><title>probe the ${esc(part.kind)}: what it holds</title>`);
      out.push(`<rect x="${rightX + 8}" y="${py}" width="174" height="24" rx="3" fill="#18191c" stroke="${probed.has(key) ? "#ff5d5d" : "#555"}" stroke-width="${probed.has(key) ? 2 : 1}"/>`);
      out.push(`<text x="${rightX + 15}" y="${py + 16}" fill="#e6e6e6" font-size="10" font-weight="700">${esc(part.kind)}</text>`);
      out.push(`<text x="${rightX + 176}" y="${py + 16}" fill="#9a9a9a" font-size="9" text-anchor="end">${esc(part.at)}${part.line ? ` · irq ${esc(part.line)}` : ""}</text></g>`);
    });
  }
  // The connectors: Ethernet, a serial console, the debug probe.
  const connectors = [];
  if (value.chip.startsWith("stm32h7")) connectors.push(["ETH", "RJ45", "the Ethernet cable: its frames"]);
  if (serial) connectors.push(["USART1", "USART1", "the serial console: its bytes, both ways"]);
  connectors.push([null, "SWD", `the debug probe: CMSIS-DAP on USB, GDB on ${value.probe ?? ""}`]);
  connectors.forEach(([probe, label, title], i) => {
    const x = 570;
    const y = 70 + i * 78;
    const on = probe && probed.has(probe);
    out.push(`<g ${probe ? `data-probe="${probe}" style="cursor:pointer"` : ""}><title>${esc(title)}</title>`);
    out.push(`<rect x="${x}" y="${y}" width="${label === "RJ45" ? 70 : 60}" height="46" rx="4" fill="${label === "RJ45" ? "#c8ccd1" : "#24272c"}" stroke="${on ? "#ff5d5d" : "#0008"}" stroke-width="${on ? 2.5 : 1}"/>`);
    if (label === "RJ45") out.push(`<rect x="${x + 17}" y="${y + 12}" width="36" height="22" rx="2" fill="#2a2d31"/>`);
    out.push(`<text x="${x + (label === "RJ45" ? 35 : 30)}" y="${y + 60}" text-anchor="middle" fill="${board.ink}" font-size="9">${esc(label === "RJ45" ? "ETH" : label)}</text></g>`);
  });
  out.push(`</svg>`);
  return out.join("");
}

/// Parts by bus, in bus order.
export function groupParts(parts) {
  const buses = new Map();
  for (const part of parts) {
    if (!buses.has(part.bus)) buses.set(part.bus, []);
    buses.get(part.bus).push(part);
  }
  return [...buses.entries()].sort(([a], [b]) => a.localeCompare(b, undefined, { numeric: true }));
}

export const partKey = (part) => `${part.bus} ${part.at}`;

/// What a probe is on: a pin's name, a bus, a USART, ETH, or a part.
export function probeKind(name) {
  if (/^P[A-K]\d+$/.test(name)) return "pin";
  if (/^I2C\d$/.test(name)) return "i2c";
  if (/^SPI\d$/.test(name)) return "spi";
  if (/^USART\d$/.test(name)) return "usart";
  if (name === "ETH") return "eth";
  return "part";
}

/// The trace channel a probe reads, if it reads one: a part's is its bus.
export function channelOf(name) {
  const kind = probeKind(name);
  return kind === "part" ? null : name;
}

// ---- what a probe shows --------------------------------------------------

/// Simulated time, in nanoseconds, as a person reads it.
export function fmtTime(ns) {
  return `${(ns / 1e9).toFixed(6)} s`;
}

/// A record's time, as a handle on that instant: the page stands the
/// board there when it is clicked (`data-board-time`, nanoseconds).
export function stamp(ns) {
  return `<span class="t" data-board-time="${ns}" title="stand the board here: every panel as it was at this instant">${fmtTime(ns)}</span>`;
}

/// A span of time, as a person reads it.
export function fmtSpan(ns) {
  if (ns >= 1e9) return `${+(ns / 1e9).toFixed(3)} s`;
  if (ns >= 1e6) return `${+(ns / 1e6).toFixed(3)} ms`;
  if (ns >= 1e3) return `${+(ns / 1e3).toFixed(3)} µs`;
  return `${Math.round(ns)} ns`;
}

/// A pin's records as a waveform over the last `window` ns before `now`:
/// an SVG, high up and low down, each edge where it happened.
export function waveform(records, now, window, level) {
  const W = 680;
  const H = 46;
  const from = now - window;
  // The level the window starts at: the last edge before it, or, if none
  // is known, the opposite of the first inside it, or the pin as it is.
  const before = records.filter((r) => r.t <= from);
  const inside = records.filter((r) => r.t > from && r.t <= now);
  let start = before.length ? before[before.length - 1].high : inside.length ? !inside[0].high : !!level;
  const x = (t) => ((t - from) / window) * W;
  const y = (high) => (high ? 8 : H - 10);
  let d = `M 0 ${y(start)}`;
  for (const r of inside) d += ` H ${x(r.t).toFixed(1)} V ${y(r.high)}`;
  d += ` H ${W}`;
  const ticks = [];
  for (let i = 0; i <= 4; i++) {
    const tx = (i / 4) * W;
    ticks.push(`<line x1="${tx}" y1="0" x2="${tx}" y2="${H}" stroke="#8884"/><text x="${Math.min(tx + 2, W - 60)}" y="${H + 10}" fill="currentColor" opacity=".6" font-size="9">${i === 4 ? "now" : `−${fmtSpan(window * (1 - i / 4))}`}</text>`);
  }
  return `<svg class="wave" viewBox="0 0 ${W} ${H + 14}" width="100%" xmlns="http://www.w3.org/2000/svg">${ticks.join("")}<path d="${d}" fill="none" stroke="#3ddc6a" stroke-width="2"/></svg>`;
}

/// I2C records grouped into transactions: from a START to the STOP, each
/// address phase with its bytes.
export function i2cTransactions(records) {
  const out = [];
  let current = null;
  for (const r of records) {
    if (r.ev === "start") {
      if (!current) current = { t: r.t, phases: [] };
      current.phases.push({ addr: r.addr, read: r.read, ack: r.ack, bytes: [] });
    } else if (r.ev === "byte") {
      if (!current) current = { t: r.t, phases: [{ addr: null, read: false, ack: true, bytes: [] }] };
      current.phases[current.phases.length - 1].bytes.push(r.b);
    } else if (r.ev === "stop") {
      if (current) {
        current.end = r.t;
        out.push(current);
      }
      current = null;
    }
  }
  if (current) out.push({ ...current, open: true });
  return out;
}

/// A transaction as a line: `0x48 W 00 · R 0c 80`.
export function i2cLine(transaction) {
  return transaction.phases
    .map((p) => `${p.addr === null ? "??" : `0x${hex2(p.addr)}`} ${p.read ? "R" : "W"}${p.ack ? "" : " NACK"}${p.bytes.length ? " " + p.bytes.map(hex2).join(" ") : ""}`)
    .join(" · ");
}

/// SPI records grouped by chip-select: from its going low to its going
/// high, the frames each way.
export function spiTransactions(records) {
  const out = [];
  let current = null;
  for (const r of records) {
    if (r.ev === "select") {
      current = { t: r.t, cs: r.cs, mosi: [], miso: [] };
    } else if (r.ev === "frame") {
      if (!current) current = { t: r.t, cs: null, mosi: [], miso: [] };
      current.mosi.push(r.mosi);
      current.miso.push(r.miso);
    } else if (r.ev === "deselect") {
      if (current) out.push({ ...current, end: r.t });
      current = null;
    }
  }
  if (current) out.push({ ...current, open: true });
  return out;
}

/// USART records as text, each way: what was sent and what was received,
/// with what is not printable escaped.
export function usartText(records, received) {
  return records
    .filter((r) => r.rx === received)
    .map((r) => (r.b === 10 ? "\n" : r.b === 13 ? "" : r.b >= 32 && r.b < 127 ? String.fromCharCode(r.b) : `\\x${hex2(r.b)}`))
    .join("");
}

/// An Ethernet frame's headers, as a line: addresses, VLAN, and what it
/// carries.
export function ethLine(record) {
  const b = (record.head.match(/../g) ?? []).map((h) => parseInt(h, 16));
  const mac = (at) => b.slice(at, at + 6).map(hex2).join(":");
  let at = 12;
  let vlan = "";
  if (b[12] === 0x81 && b[13] === 0x00) {
    vlan = ` VLAN 0x${(((b[14] << 8) | b[15]) & 0xfff).toString(16)}`;
    at = 16;
  }
  const type = (b[at] << 8) | b[at + 1];
  let what = `ethertype 0x${type.toString(16).padStart(4, "0")}`;
  if (type === 0x86dd && b.length >= at + 42) {
    const ip = at + 2;
    const v6 = (o) => {
      const groups = [];
      for (let i = 0; i < 16; i += 2) groups.push((b[o + i] << 8) | b[o + i + 1]);
      return ipv6(groups);
    };
    const next = b[ip + 6];
    const body = ip + 40;
    let upper = `next header ${next}`;
    if (next === 17) upper = `UDP ${(b[body] << 8) | b[body + 1]} → ${(b[body + 2] << 8) | b[body + 3]}`;
    if (next === 58) upper = `ICMPv6 ${{ 128: "echo request", 129: "echo reply", 135: "neighbour solicitation", 136: "neighbour advertisement" }[b[body]] ?? `type ${b[body]}`}`;
    what = `IPv6 ${v6(ip + 8)} → ${v6(ip + 24)} ${upper}`;
  }
  return `${mac(6)} → ${mac(0)}${vlan} · ${what}`;
}

/// An IPv6 address from its eight groups: the longest run of two or more
/// zero groups as `::`.
export function ipv6(groups) {
  let best = [0, 0];
  for (let i = 0; i < 8; ) {
    let j = i;
    while (j < 8 && groups[j] === 0) j++;
    if (j - i > best[1] - best[0]) best = [i, j];
    i = j > i ? j : i + 1;
  }
  const hex = (gs) => gs.map((g) => g.toString(16)).join(":");
  if (best[1] - best[0] < 2) return hex(groups);
  return `${hex(groups.slice(0, best[0]))}::${hex(groups.slice(best[1]))}`;
}

/// What a probe shows, as HTML, from its records and the board's state.
export function probeBody(name, records, value, window) {
  const kind = probeKind(name);
  const now = value.simulated_ms * 1e6;
  const newest = (list, n = 40) => list.slice(-n).reverse();
  if (kind === "pin") {
    const state = pinState(value, name);
    const role = pinRoles(value).get(name);
    const edges = newest(records, 8).map((r) => `<div>${stamp(r.t)} ${r.high ? "↑ high" : "↓ low"}</div>`).join("");
    return `<div class="dim">${state ? `${state.mode}, now ${state.high ? "high" : "low"}` : ""}${role ? ` — ${esc(role)}` : ""} · ${records.length} edges seen</div>${waveform(records, now, window, state?.high)}<div class="edges">${edges || '<div class="dim">no edges yet</div>'}</div>`;
  }
  if (kind === "i2c") {
    const rows = newest(i2cTransactions(records)).map((t) => `<div>${stamp(t.t)} ${esc(i2cLine(t))}${t.open ? " …" : ""}</div>`);
    return rows.join("") || `<div class="dim">no transactions yet</div>`;
  }
  if (kind === "spi") {
    const parts = new Map((value.parts ?? []).filter((p) => p.bus === name).map((p) => [p.at, p.kind]));
    const rows = newest(spiTransactions(records)).map((t) => {
      const who = t.cs ? `${t.cs}${parts.get(t.cs) ? ` ${parts.get(t.cs)}` : ""}` : "—";
      const cut = (bytes) => bytes.slice(0, 24).map(hex2).join(" ") + (bytes.length > 24 ? ` … (${bytes.length})` : "");
      return `<div>${stamp(t.t)} <b>${esc(who)}</b><div class="frames">→ ${cut(t.mosi) || "—"}<br>← ${cut(t.miso) || "—"}</div></div>`;
    });
    return rows.join("") || `<div class="dim">no transfers yet</div>`;
  }
  if (kind === "usart") {
    const sent = usartText(records, false).slice(-3000);
    const got = usartText(records, true).slice(-1000);
    return `<div class="dim">sent</div><pre class="serial">${esc(sent) || "—"}</pre><div class="dim">received</div><pre class="serial">${esc(got) || "—"}</pre>`;
  }
  if (kind === "eth") {
    const rows = newest(records).map((r) => `<div>${stamp(r.t)} ${r.rx ? "⇠ in " : "⇢ out"} ${r.len} B · ${esc(ethLine(r))}</div>`);
    return rows.join("") || `<div class="dim">no frames yet</div>`;
  }
  const part = (value.parts ?? []).find((p) => partKey(p) === name);
  if (!part) return `<div class="dim">not on the board now</div>`;
  const rows = Object.entries(part.values).map(([k, v]) => `<tr><td class="key">${esc(k)}</td><td>${v.includes("\n") ? `<pre>${esc(v)}</pre>` : esc(v)}</td></tr>`);
  return `<div class="dim">${esc(part.kind)} on ${esc(part.bus)} at ${esc(part.at)}${part.line ? `, its interrupt on ${esc(part.line)}` : ""}</div><table class="values">${rows.join("")}</table>`;
}

// ---- the board's timeline ---------------------------------------------------
//
// What the core did and what crossed the buses, in the board's own time, as
// lanes: which Hubris task ran, what it asked the kernel (each supervisor
// call), the exceptions taken (SysTick, interrupts, faults), and each bus's
// and the cable's traffic; the board's resets and halts across them all.

/// Hubris's system calls, by the number a task puts in r11
/// (`sys/abi/src/lib.rs`, `Sysnum`).
export const SYSCALLS = [
  "send", "recv", "reply", "set_timer", "borrow_read", "borrow_write", "borrow_info",
  "irq_control", "panic", "get_timer", "refresh_task_id", "post", "reply_fault", "irq_status",
];

/// What a send to the kernel asks of it, by operation (`Kipcnum`).
export const KIPC = {
  1: "read_task_status", 2: "reinit_task", 3: "fault_task", 4: "read_image_id", 5: "reset",
  6: "get_task_dump_region", 7: "read_task_dump_region", 8: "software_irq", 9: "find_faulted_task",
  10: "read_panic_message",
};

/// The kernel's task id: all ones.
const KERNEL_ID = 0xffff;
const INDEX_MASK = 0x3ff;

/// A system call as a line: `send → i2c_driver op 1`, `kipc reinit_task`,
/// `recv`, with a `kind` to colour it by. `taskName(index)` names a task.
export function syscallLine({ r11, r4, r5 }, taskName = (n) => `task ${n}`) {
  const name = SYSCALLS[r11] ?? `syscall ${r11}`;
  const target = (id) => (id === KERNEL_ID ? "the kernel" : taskName(id & INDEX_MASK));
  switch (name) {
    case "send": {
      const to = r4 >>> 16;
      const op = r4 & 0xffff;
      if (to === KERNEL_ID) return { name, kind: "kernel", text: `kipc ${KIPC[op] ?? `op ${op}`}` };
      return { name, kind: "send", text: `send → ${target(to)} op ${op}` };
    }
    case "reply":
    case "reply_fault":
      return { name, kind: "reply", text: `${name} → ${target(r4 & 0xffff)}` };
    case "post":
      return { name, kind: "send", text: `post → ${target(r4 & 0xffff)} bits 0x${(r5 >>> 0).toString(16)}` };
    case "panic":
      return { name, kind: "fault", text: "panic" };
    case "recv":
      return { name, kind: "recv", text: "recv" };
    default:
      return { name, kind: "other", text: name };
  }
}

/// The timeline's layers, each one a set of the trace's channels.
export const LAYERS = {
  tasks: ["TASK"],
  syscalls: ["SYS"],
  interrupts: ["EXC"],
  devices: ["I2C1", "I2C2", "I2C3", "I2C4", "SPI1", "SPI2", "SPI3", "SPI4", "SPI5", "SPI6", "USART1", "USART2", "USART3", "ETH"],
};

const FAULTS = new Set([3, 4, 5, 6]);

/// A window of records — `[from, to)`, nanoseconds — as lanes to draw:
/// `{ name, spans: [{ from, to, label, color }], marks: [{ t, text, kind }] }`.
/// `records` is every record kept, by channel — a task lane starts the
/// window with the task the last change before it named — and `layers`
/// says which are on.
export function timelineLanes(records, layers, from, to, taskName = (n) => `task ${n}`) {
  const within = (list) => (list ?? []).filter((r) => r.t >= from && r.t < to);
  const lanes = [];
  if (layers.tasks) {
    const all = records.get("TASK") ?? [];
    const before = all.filter((r) => r.t < from).at(-1);
    const changes = [...(before ? [{ ...before, t: from }] : []), ...within(all)];
    const spans = changes.map((r, i) => ({
      from: r.t,
      to: i + 1 < changes.length ? changes[i + 1].t : to,
      task: r.task,
      label: r.task === null ? "?" : taskName(r.task),
      color: taskColor(r.task),
    }));
    lanes.push({ name: "tasks", spans, marks: [] });
  }
  if (layers.syscalls) {
    const marks = within(records.get("SYS")).map((r) => {
      const line = syscallLine(r, taskName);
      return { t: r.t, text: line.text, kind: line.kind };
    });
    lanes.push({ name: "syscalls", spans: [], marks });
  }
  if (layers.interrupts) {
    const marks = within(records.get("EXC")).map((r) => ({
      t: r.t,
      text: r.name,
      kind: FAULTS.has(r.exc) ? "fault" : r.exc === 15 ? "tick" : "irq",
    }));
    lanes.push({ name: "interrupts", spans: [], marks });
  }
  if (layers.devices) {
    for (const channel of LAYERS.devices) {
      const list = within(records.get(channel));
      if (!list.length && !(records.get(channel) ?? []).length) continue;
      const marks = list.map((r) => ({ t: r.t, text: deviceText(channel, r), kind: "device" }));
      lanes.push({ name: channel, spans: [], marks });
    }
  }
  const board = within(records.get("BOARD")).map((r) => ({ t: r.t, text: r.what, kind: "board" }));
  return { lanes, board };
}

function deviceText(channel, r) {
  if (channel.startsWith("I2C")) return r.ev === "start" ? `START 0x${r.addr.toString(16)} ${r.read ? "R" : "W"}${r.ack ? "" : " NAK"}` : r.ev === "byte" ? `byte ${hex2(r.b)}` : "STOP";
  if (channel.startsWith("SPI")) return r.ev === "frame" ? `${hex2(r.mosi)} → ← ${hex2(r.miso)}` : `${r.ev} ${r.cs}`;
  if (channel === "ETH") return `${r.rx ? "in" : "out"} ${r.len} B · ${ethLine(r)}`;
  return `${r.rx ? "in" : "out"} ${hex2(r.b)}`;
}

/// A task's colour: its own hue, the same everywhere.
export function taskColor(index) {
  if (index === null || index === undefined) return "#666";
  return `hsl(${(index * 47) % 360} 55% 52%)`;
}
