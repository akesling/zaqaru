// USB devices from the browser, granted to a container as a store.
//
// The container's kernel reaches USB devices through a store mounted at
// `services/usb` (see `crates/kernel/src/usb.rs` for the contract): it lists
// `devices`, reads each one's `descriptors`, `strings/{n}` and `speed`,
// writes OUT transfers to `endpoints/{ep}/tx` and asks
// `endpoints/{ep}/pending/{max}` for IN transfers, which is absent until one
// has arrived. This is that store over WebUSB, so the stock programs inside
// — probe-rs, libusb — find a device on the desk as they would one plugged
// into the machine they think they are on.
//
// WebUSB is asynchronous and the store is not, so the two meet in buffers:
//
// - What the kernel reads once and keeps — descriptors, strings — is read
//   off the device when it is attached, before the kernel can ask.
// - An OUT transfer is queued to the device and answered at once; WebUSB
//   keeps an endpoint's transfers in order, so a later transfer cannot pass
//   it.
// - An IN read starts a transfer if none is outstanding on that endpoint,
//   and answers absent; when it completes its bytes wait for the next read,
//   and `onArrival` is called so the host can wake a container that parked
//   waiting for them.
//
// Requesting a device needs a user's gesture, which a worker has none of:
// the page asks (`request`), and the worker, where the container runs,
// picks up what was granted (`refresh`). A device's id is its serial number
// where it has one, so it survives being unplugged and granted again.
//
// Replay is the recording's business, not this store's: a checkpoint's copy
// of the mount table answers reads from the tape. A copy of this store
// (`snapshot`) is therefore inert — it takes writes and sends nothing — so
// re-executing a stretch of history never sends a real device the commands
// it was sent the first time.

const decoder = new TextDecoder();
const encoder = new TextEncoder();
const text = (segment) => decoder.decode(segment);

/// The page's half: asks the user for a device. Any device, since what is
/// worth granting is the user's call; the container decides what it can use.
export async function request() {
  if (!navigator.usb) throw "this browser has no WebUSB";
  const device = await navigator.usb.requestDevice({ filters: [] });
  return describe(device);
}

function describe(device) {
  return {
    vendor: device.vendorId,
    product: device.productId,
    name: device.productName ?? "",
    manufacturer: device.manufacturerName ?? "",
    serial: device.serialNumber ?? "",
  };
}

export class WebUsb {
  constructor({ onArrival = () => {}, onChange = () => {} } = {}) {
    this.devices = new Map(); // id -> attached device
    this.onArrival = onArrival;
    this.onChange = onChange;
    if (globalThis.navigator?.usb) {
      navigator.usb.addEventListener("disconnect", (event) => this.detach(event.device));
      navigator.usb.addEventListener("connect", () => this.refresh());
    }
  }

  /// Attaches every device this origin has been granted and is not attached
  /// yet. Answers what is attached.
  async refresh() {
    if (!globalThis.navigator?.usb) return this.list();
    for (const device of await navigator.usb.getDevices()) {
      if ([...this.devices.values()].some((held) => held.device === device)) continue;
      try {
        await this.attach(device);
      } catch (why) {
        console.warn(`zaqaru: could not attach ${device.productName ?? "a USB device"}: ${why}`);
      }
    }
    this.onChange(this.list());
    return this.list();
  }

  list() {
    return [...this.devices].map(([id, held]) => ({ id, ...describe(held.device), claimed: held.claimed, error: held.error ?? null }));
  }

  async attach(device) {
    await device.open();
    if (device.configuration === null) await device.selectConfiguration(1);
    const get = async (value, index, length) => {
      const result = await device.controlTransferIn({ requestType: "standard", recipient: "device", request: 0x06, value, index }, length);
      if (result.status !== "ok") throw `GET_DESCRIPTOR ${value.toString(16)}: ${result.status}`;
      return new Uint8Array(result.data.buffer, result.data.byteOffset, result.data.byteLength);
    };
    const head = await get(0x0100, 0, 18);
    const configurationHead = await get(0x0200, 0, 9);
    const total = configurationHead[2] | (configurationHead[3] << 8);
    const configuration = await get(0x0200, 0, total);
    const descriptors = new Uint8Array(head.length + configuration.length);
    descriptors.set(head);
    descriptors.set(configuration, head.length);

    // The strings the descriptors name: the device's three, and each
    // interface's, which is how a CMSIS-DAP probe is told from the rest.
    const indices = new Set([head[14], head[15], head[16]]);
    for (let at = 0; at + 1 < configuration.length && configuration[at] >= 2; at += configuration[at]) {
      if (configuration[at + 1] === 4) indices.add(configuration[at + 8]);
    }
    indices.delete(0);
    const strings = new Map();
    for (const index of indices) {
      try {
        const d = await get(0x0300 | index, 0x0409, 255);
        strings.set(index, String.fromCharCode(...new Uint16Array(d.slice(2, d[0] & ~1).buffer)));
      } catch {
        // A string the device will not give is a string sysfs leaves out.
      }
    }

    // Claimed now, while the user is here to see it fail: an interface the
    // browser keeps for itself (HID, mass storage) is simply not claimed.
    const claimed = [];
    for (const iface of device.configuration.interfaces) {
      try {
        await device.claimInterface(iface.interfaceNumber);
        claimed.push(iface.interfaceNumber);
      } catch {}
    }
    const id = `webusb-${device.serialNumber || `${device.vendorId.toString(16)}-${device.productId.toString(16)}-${this.devices.size}`}`;
    this.devices.set(id, {
      device,
      descriptors,
      strings,
      // WebUSB does not say; a USB 2.0 device is taken to be high speed.
      speed: (head[2] | (head[3] << 8)) >= 0x0200 ? "480" : "12",
      claimed,
      arrived: new Map(), // endpoint -> [bytes]
      outstanding: new Set(), // endpoints with an IN transfer under way
      control: null,
      error: null,
    });
  }

  detach(device) {
    for (const [id, held] of this.devices) if (held.device === device) this.devices.delete(id);
    this.onChange(this.list());
  }

  /// `services/usb/...` reads.
  read(path) {
    const rest = path.slice(2).map(text);
    if (rest.length === 1 && rest[0] === "devices") return encoder.encode([...this.devices.keys()].join("\n"));
    if (rest[0] !== "devices") return null;
    const held = this.devices.get(rest[1]);
    if (!held) return null;
    const [, , what, ...more] = rest;
    if (what === "descriptors") return held.descriptors;
    if (what === "speed") return encoder.encode(held.speed);
    if (what === "strings") {
      const string = held.strings.get(Number(more[0]));
      return string === undefined ? null : encoder.encode(string);
    }
    if (what === "control") {
      const reply = held.control;
      held.control = null;
      if (reply?.error) throw reply.error;
      return reply?.data ?? null;
    }
    if (what === "endpoints" && more[1] === "pending") {
      const endpoint = parseInt(more[0], 16);
      const queue = held.arrived.get(endpoint);
      if (queue?.length) return queue.shift();
      if (!held.outstanding.has(endpoint)) this.receive(held, endpoint, Number(more[2]));
      return null;
    }
    return null;
  }

  receive(held, endpoint, max) {
    held.outstanding.add(endpoint);
    held.device
      .transferIn(endpoint & 0x7f, max)
      .then((result) => {
        const data = result.data ? new Uint8Array(result.data.buffer, result.data.byteOffset, result.data.byteLength).slice() : new Uint8Array();
        if (!held.arrived.has(endpoint)) held.arrived.set(endpoint, []);
        held.arrived.get(endpoint).push(data);
      })
      .catch((why) => (held.error = String(why)))
      .finally(() => {
        held.outstanding.delete(endpoint);
        this.onArrival();
      });
  }

  /// `services/usb/...` writes. Answers the path written, as a store does.
  write(path, data) {
    const rest = path.slice(2).map(text);
    const held = rest[0] === "devices" ? this.devices.get(rest[1]) : null;
    if (!held) throw `no USB device ${rest[1]}`;
    const [, , what, ...more] = rest;
    if (what === "endpoints" && more[1] === "tx") {
      const bytes = data.slice();
      held.device.transferOut(parseInt(more[0], 16) & 0x7f, bytes).catch((why) => (held.error = String(why)));
      return path;
    }
    if (what === "control") {
      // A class or vendor request. The kernel reads the reply straight
      // after writing the request, and WebUSB cannot answer that fast: the
      // first read finds nothing, which the kernel takes as a stall.
      // Recorded here so a request that is retried finds its answer.
      const setup = { requestType: ["standard", "class", "vendor"][(data[0] >> 5) & 3], recipient: ["device", "interface", "endpoint", "other"][data[0] & 3], request: data[1], value: data[2] | (data[3] << 8), index: data[4] | (data[5] << 8) };
      const length = data[6] | (data[7] << 8);
      const done = data[0] & 0x80 ? held.device.controlTransferIn(setup, length) : held.device.controlTransferOut(setup, data.slice(8));
      done
        .then((result) => (held.control = { data: result.data ? new Uint8Array(result.data.buffer, result.data.byteOffset, result.data.byteLength).slice() : new Uint8Array() }))
        .catch((why) => (held.control = { error: String(why) }))
        .finally(() => this.onArrival());
      return path;
    }
    throw `${rest.join("/")} is not writable`;
  }

  /// A copy for a checkpoint, which answers reads from the tape: inert, so
  /// re-executing history sends a real device nothing.
  snapshot() {
    return { read: () => null, write: (path) => path, snapshot() { return this; }, save: () => ({ kind: "usb" }) };
  }

  save() {
    return { kind: "usb" };
  }
}
