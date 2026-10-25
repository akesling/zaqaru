//! USB devices: the store a provider serves them through, and Linux's
//! `usbfs` over it.
//!
//! # The contract
//!
//! A USB device reaches a container the way everything else does: as paths
//! in a store. A provider is granted at [`crate::paths::USB`] — the
//! `services/usb` name the isotope spec suggests for a granted service,
//! rather than another `iso/` branch — and serves, for each device it
//! offers:
//!
//! | path                                   | operation                                         |
//! |----------------------------------------|---------------------------------------------------|
//! | `devices`                              | read: the device ids, one per line                |
//! | `devices/{id}/descriptors`             | read: the device descriptor, then each configuration's |
//! | `devices/{id}/strings/{n}`             | read: string descriptor `n`, as UTF-8             |
//! | `devices/{id}/speed`                   | read: the link's speed in Mbit/s, as sysfs gives it |
//! | `devices/{id}/control`                 | write: a class or vendor request, the setup packet then its OUT data; read: its IN data. Absent or failed is a stall |
//! | `devices/{id}/endpoints/{ep}/tx`       | write: one OUT transfer's bytes                   |
//! | `devices/{id}/endpoints/{ep}/pending/{max}` | read: one IN transfer of at most `max` bytes, or absent while there is none |
//!
//! `{ep}` is the endpoint address in two hex digits (`01`, `81`). An
//! endpoint is the binary-stream profile's two halves with packet
//! boundaries kept — a transfer in is a transfer out — and `pending` is its
//! non-blocking read, as `requests/pending` is the server protocol's: the
//! kernel never waits on a store, so it asks again when it next could.
//!
//! The board's probe ([`crate::board`]) is one provider, served by the
//! kernel itself under the id [`BOARD`]; every other id is the host's, so a
//! host that grants `services/usb` can hand a container real devices.
//!
//! # The face
//!
//! What a program sees is what Linux shows it, built from that contract:
//! `/sys/bus/usb/devices/1-N` with the attributes libusb and nusb read
//! (see [`Usb::attribute`]), and `/dev/bus/usb/001/00M`, whose descriptor
//! reads the raw descriptors and takes `usbfs`'s ioctls. Transfers are
//! URBs: submitted with the guest's own `struct usbdevfs_urb`, completed
//! into kernel memory, and written back into the guest's only when it reaps
//! them, in its own turn — the same split as every other wait here. A
//! descriptor with a completed URB polls writable, which is how `usbfs`
//! says so.
//!
//! The standard requests are answered from the descriptors, as Linux
//! answers them from its cached copy; only the rest go to the provider.

use std::collections::VecDeque;

use crate::abi::StoreOutcome;
use crate::errno::Errno;
use crate::syscall::{Arguments, Outcome};

/// The provider id of the board's own probe.
pub const BOARD: &str = "board";

/// `usbfs`'s character major.
pub const MAJOR: u32 = 189;

/// The bus every device is on: one root hub, address 1, and the devices on
/// its ports from address 2.
pub const BUS: u8 = 1;

pub mod ioctl {
    pub const CONTROL: u64 = 0xC018_5500;
    pub const SETINTERFACE: u64 = 0x8008_5504;
    pub const SETCONFIGURATION: u64 = 0x8004_5505;
    pub const SUBMITURB: u64 = 0x8038_550A;
    pub const DISCARDURB: u64 = 0x0000_550B;
    pub const REAPURB: u64 = 0x4008_550C;
    pub const REAPURBNDELAY: u64 = 0x4008_550D;
    pub const CLAIMINTERFACE: u64 = 0x8004_550F;
    pub const RELEASEINTERFACE: u64 = 0x8004_5510;
    pub const CONNECTINFO: u64 = 0x4008_5511;
    pub const IOCTL: u64 = 0xC010_5512;
    pub const RESET: u64 = 0x0000_5514;
    pub const CLEAR_HALT: u64 = 0x8004_5515;
    pub const GET_CAPABILITIES: u64 = 0x8004_551A;
    pub const DISCONNECT_CLAIM: u64 = 0x8108_551B;
    pub const GET_SPEED: u64 = 0x0000_551F;
}

/// `struct usbdevfs_urb`, and the fields of it read and written.
const URB: usize = 56;
const URB_STATUS: u64 = 4;
const URB_ACTUAL: u64 = 28;
const TYPE_CONTROL: u8 = 2;
const TYPE_BULK: u8 = 3;
const TYPE_INTERRUPT: u8 = 1;

/// `USBDEVFS_CAP_NO_PACKET_SIZE_LIM` and `USBDEVFS_CAP_REAP_AFTER_DISCONNECT`.
const CAPABILITIES: u32 = 0x04 | 0x10;
/// `USB_SPEED_HIGH`.
const SPEED_HIGH: i64 = 3;

/// A device a provider offers.
#[derive(Clone, Debug)]
pub struct Device {
    pub id: String,
    pub address: u8,
    pub descriptors: Vec<u8>,
    /// String descriptors by index, as the provider gave them.
    pub strings: Vec<(u8, String)>,
    pub speed: String,
}

impl Device {
    fn byte(&self, at: usize) -> u8 {
        self.descriptors.get(at).copied().unwrap_or(0)
    }

    fn half(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.byte(at), self.byte(at + 1)])
    }

    pub fn string(&self, index: u8) -> Option<&str> {
        self.strings.iter().find(|(i, _)| *i == index).map(|(_, s)| s.as_str())
    }

    /// The descriptors after the device's: every configuration, and their
    /// interfaces and endpoints, in order.
    fn rest(&self) -> &[u8] {
        self.descriptors.get(18..).unwrap_or(&[])
    }

    /// The interface descriptors of the first configuration.
    pub fn interfaces(&self) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let mut at = 0;
        let rest = self.rest();
        while at + 2 <= rest.len() && rest[at] >= 2 {
            let length = rest[at] as usize;
            if rest[at + 1] == 4 {
                out.push(&rest[at..(at + length).min(rest.len())]);
            }
            at += length;
        }
        out
    }

    /// The string indices the descriptors refer to.
    pub fn string_indices(&self) -> Vec<u8> {
        let mut indices = vec![self.byte(14), self.byte(15), self.byte(16)];
        indices.extend(self.interfaces().iter().filter_map(|i| i.get(8).copied()));
        indices.retain(|&i| i != 0);
        indices.dedup();
        indices
    }

    /// A string descriptor, as GET_DESCRIPTOR(STRING) returns it.
    fn string_descriptor(&self, index: u8) -> Option<Vec<u8>> {
        if index == 0 {
            return Some(vec![4, 3, 0x09, 0x04]);
        }
        let units: Vec<u16> = self.string(index)?.encode_utf16().take(126).collect();
        let mut d = vec![2 + 2 * units.len() as u8, 3];
        for unit in units {
            d.extend(unit.to_le_bytes());
        }
        Some(d)
    }
}

/// One URB the guest submitted.
#[derive(Clone, Debug)]
struct Urb {
    /// The guest's `struct usbdevfs_urb`, which reaping hands back.
    user: u64,
    endpoint: u8,
    /// Where the IN data goes when it is reaped: the buffer, or for a
    /// control transfer the buffer past its setup packet.
    into: u64,
    length: u32,
    status: i32,
    actual: u32,
    data: Vec<u8>,
}

/// An open device file: which device, and its URBs.
#[derive(Clone, Debug, Default)]
pub(crate) struct Handle {
    device: usize,
    pending: Vec<Urb>,
    completed: VecDeque<Urb>,
    references: u32,
}

/// One transfer, as the log keeps it: when, which way, on which endpoint,
/// how it ended, and the first bytes of it — enough to say what a probe was
/// asked, which a CMSIS-DAP command says in its first few.
#[derive(Clone, Debug)]
pub struct Transfer {
    /// The container's instant, in retired instructions.
    pub at: u64,
    pub endpoint: u8,
    pub length: u32,
    pub status: i32,
    pub head: Vec<u8>,
}

/// What a device has carried: totals, and the latest transfers.
#[derive(Clone, Debug, Default)]
pub struct Log {
    pub out: (u64, u64),
    pub inward: (u64, u64),
    pub recent: VecDeque<Transfer>,
}

/// How many transfers a device's log keeps, and how much of each.
pub const RECENT: usize = 48;
pub const HEAD: usize = 24;

impl Log {
    fn note(&mut self, transfer: Transfer) {
        let totals = if transfer.endpoint & 0x80 != 0 { &mut self.inward } else { &mut self.out };
        totals.0 += 1;
        totals.1 += u64::from(transfer.length);
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(transfer);
    }
}

/// Every device and open device file in the container, and what each
/// device has carried — kept by id across enumerations, which happen at
/// every `exec`, so the log is the device's and not one program's.
#[derive(Clone, Debug, Default)]
pub struct Usb {
    pub devices: Vec<Device>,
    pub(crate) handles: Vec<Option<Handle>>,
    pub logs: std::collections::BTreeMap<String, Log>,
}

/// See [`crate::ring::Shared`].
pub type Shared = std::rc::Rc<std::cell::RefCell<Usb>>;

impl Usb {
    /// The device with this minor number, which is its address less one.
    pub fn by_minor(&self, minor: u32) -> Option<usize> {
        self.devices.iter().position(|d| u32::from(d.address) == minor + 1)
    }

    fn open(&mut self, device: usize) -> u32 {
        let handle = Handle { device, references: 1, ..Handle::default() };
        if let Some(free) = self.handles.iter().position(Option::is_none) {
            self.handles[free] = Some(handle);
            return free as u32;
        }
        self.handles.push(Some(handle));
        (self.handles.len() - 1) as u32
    }

    pub fn acquire(&mut self, id: u32) {
        if let Some(Some(handle)) = self.handles.get_mut(id as usize) {
            handle.references += 1;
        }
    }

    /// The last descriptor is gone: its URBs go with it, so a transfer
    /// pending for a process that has ended never takes data meant for the
    /// next one.
    pub fn release(&mut self, id: u32) {
        let Some(Some(handle)) = self.handles.get_mut(id as usize) else { return };
        handle.references = handle.references.saturating_sub(1);
        if handle.references == 0 {
            self.handles[id as usize] = None;
        }
    }

    pub fn reapable(&self, id: u32) -> bool {
        self.handle(id).is_some_and(|h| !h.completed.is_empty())
    }

    fn handle(&self, id: u32) -> Option<&Handle> {
        self.handles.get(id as usize).and_then(Option::as_ref)
    }

    fn handle_mut(&mut self, id: u32) -> Option<&mut Handle> {
        self.handles.get_mut(id as usize).and_then(Option::as_mut)
    }

    /// The IN transfers waiting, at most one per endpoint and the oldest of
    /// each: a provider's transfers arrive in order.
    fn waiting(&self) -> Vec<(u32, String, u8, u32)> {
        let mut out = Vec::new();
        for (id, handle) in self.handles.iter().enumerate() {
            let Some(handle) = handle else { continue };
            let Some(device) = self.devices.get(handle.device) else { continue };
            let mut seen = Vec::new();
            for urb in &handle.pending {
                if !seen.contains(&urb.endpoint) {
                    seen.push(urb.endpoint);
                    out.push((id as u32, device.id.clone(), urb.endpoint, urb.length));
                }
            }
        }
        out
    }

    /// Completes the oldest pending URB on `endpoint` with `data`.
    fn complete(&mut self, id: u32, endpoint: u8, status: i32, data: Vec<u8>, at: u64) {
        let Some(handle) = self.handle_mut(id) else { return };
        let Some(index) = handle.pending.iter().position(|u| u.endpoint == endpoint) else { return };
        let mut urb = handle.pending.remove(index);
        urb.status = status;
        urb.actual = data.len().min(urb.length as usize) as u32;
        let transfer = Transfer { at, endpoint, length: urb.actual, status, head: data[..data.len().min(HEAD)].to_vec() };
        urb.data = data;
        handle.completed.push_back(urb);
        let device = handle.device;
        self.note(device, transfer);
    }

    /// Logs a transfer against device `device`.
    fn note(&mut self, device: usize, transfer: Transfer) {
        if let Some(id) = self.devices.get(device).map(|d| d.id.clone()) {
            self.logs.entry(id).or_default().note(transfer);
        }
    }

    /// The table as the store's `usb` lens serves it.
    pub fn json(&self) -> String {
        use crate::system::server::quoted;
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let devices: Vec<String> = self
            .devices
            .iter()
            .enumerate()
            .map(|(n, d)| {
                let log = self.logs.get(&d.id).cloned().unwrap_or_default();
                let recent: Vec<String> = log
                    .recent
                    .iter()
                    .map(|t| {
                        format!(
                            r#"{{"at":{},"endpoint":"{:02x}","length":{},"status":{},"head":"{}"}}"#,
                            t.at,
                            t.endpoint,
                            t.length,
                            t.status,
                            hex(&t.head)
                        )
                    })
                    .collect();
                let attribute = |name| self.attribute(n, None, name).map_or("null".to_string(), |v| quoted(&v));
                let interface = self.attribute(n, Some(0), "interface").map_or("null".to_string(), |v| quoted(&v));
                let open = self.handles.iter().flatten().filter(|h| h.device == n).count();
                format!(
                    r#"{{"id":{},"provider":{},"bus":{},"address":{},"vendor":{},"product":{},"manufacturer":{},"name":{},"serial":{},"interface":{},"speed":{},"open":{},"out":{{"transfers":{},"bytes":{}}},"in":{{"transfers":{},"bytes":{}}},"recent":[{}]}}"#,
                    quoted(&d.id),
                    quoted(if d.id == BOARD { "board" } else { "host" }),
                    BUS,
                    d.address,
                    attribute("idVendor"),
                    attribute("idProduct"),
                    attribute("manufacturer"),
                    attribute("product"),
                    attribute("serial"),
                    interface,
                    quoted(&d.speed),
                    open,
                    log.out.0,
                    log.out.1,
                    log.inward.0,
                    log.inward.1,
                    recent.join(",")
                )
            })
            .collect();
        format!(r#"{{"devices":[{}]}}"#, devices.join(","))
    }

    /// A sysfs attribute of device `device`, or of its interface
    /// `interface`, as `/sys/bus/usb/devices` renders it.
    pub fn attribute(&self, device: usize, interface: Option<usize>, name: &str) -> Option<String> {
        let d = self.devices.get(device)?;
        if let Some(n) = interface {
            let i = *d.interfaces().get(n)?;
            let at = |k: usize| i.get(k).copied().unwrap_or(0);
            return Some(match name {
                "bInterfaceNumber" => format!("{:02x}", at(2)),
                "bAlternateSetting" => format!("{:2}", at(3)),
                "bNumEndpoints" => format!("{:02x}", at(4)),
                "bInterfaceClass" => format!("{:02x}", at(5)),
                "bInterfaceSubClass" => format!("{:02x}", at(6)),
                "bInterfaceProtocol" => format!("{:02x}", at(7)),
                "interface" => d.string(at(8))?.to_string(),
                _ => return None,
            });
        }
        let bcd = |v: u16| format!("{:x}.{:02x}", v >> 8, v & 0xFF);
        Some(match name {
            "busnum" => BUS.to_string(),
            "devnum" => d.address.to_string(),
            "devpath" => (d.address - 1).to_string(),
            "idVendor" => format!("{:04x}", d.half(8)),
            "idProduct" => format!("{:04x}", d.half(10)),
            "bcdDevice" => format!("{:04x}", d.half(12)),
            "version" => format!("{:>5}", bcd(d.half(2))),
            "bDeviceClass" => format!("{:02x}", d.byte(4)),
            "bDeviceSubClass" => format!("{:02x}", d.byte(5)),
            "bDeviceProtocol" => format!("{:02x}", d.byte(6)),
            "bMaxPacketSize0" => d.byte(7).to_string(),
            "bNumConfigurations" => d.byte(17).to_string(),
            "bConfigurationValue" => d.rest().get(5).copied().unwrap_or(1).to_string(),
            "bNumInterfaces" => format!("{:2}", d.interfaces().len()),
            "speed" => d.speed.clone(),
            "manufacturer" => d.string(d.byte(14))?.to_string(),
            "product" => d.string(d.byte(15))?.to_string(),
            "serial" => d.string(d.byte(16))?.to_string(),
            _ => return None,
        })
    }
}

/// The attributes rendered for a device, and for each interface.
pub const DEVICE_ATTRIBUTES: &[&str] = &[
    "busnum",
    "devnum",
    "devpath",
    "idVendor",
    "idProduct",
    "bcdDevice",
    "version",
    "bDeviceClass",
    "bDeviceSubClass",
    "bDeviceProtocol",
    "bMaxPacketSize0",
    "bNumConfigurations",
    "bConfigurationValue",
    "bNumInterfaces",
    "speed",
    "manufacturer",
    "product",
    "serial",
];
pub const INTERFACE_ATTRIBUTES: &[&str] = &[
    "bInterfaceNumber",
    "bAlternateSetting",
    "bNumEndpoints",
    "bInterfaceClass",
    "bInterfaceSubClass",
    "bInterfaceProtocol",
    "interface",
];

fn endpoint_name(endpoint: u8) -> Vec<u8> {
    format!("{endpoint:02x}").into_bytes()
}

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// The container's instant, as the running thread counts it: see
    /// `cpu::state::Tcb::clock`.
    fn instant(&mut self) -> u64 {
        self.machine.tcb().map_or(0, |thread| thread.retired.wrapping_add(thread.clock))
    }

    /// Reads `rest` of device `id` from whoever provides it.
    fn usb_read(&mut self, id: &str, rest: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        if id == BOARD {
            return match &mut *self.board.borrow_mut() {
                crate::board::Slot::Present(board) => board.usb_read(rest, into),
                _ => StoreOutcome::Failed,
            };
        }
        let mut path: Vec<&[u8]> = crate::paths::USB.to_vec();
        path.extend([&b"devices"[..], id.as_bytes()]);
        path.extend_from_slice(rest);
        self.store.read(&path, into)
    }

    fn usb_write(&mut self, id: &str, rest: &[&[u8]], data: &[u8]) -> StoreOutcome {
        if id == BOARD {
            // The board runs beside its debugger, not only when the
            // container is between slices: a debugger polling a core it has
            // just let run, a transfer at a time, is otherwise polling a
            // board that never gets the time to do anything.
            let now = self.monotonic();
            return match &mut *self.board.borrow_mut() {
                crate::board::Slot::Present(board) => {
                    if let Some(now) = now {
                        board.catch_up(now);
                    }
                    board.usb_write(rest, data)
                }
                _ => StoreOutcome::Failed,
            };
        }
        let mut path: Vec<&[u8]> = crate::paths::USB.to_vec();
        path.extend([&b"devices"[..], id.as_bytes()]);
        path.extend_from_slice(rest);
        self.store.write(&path, data)
    }

    /// Asks every provider what it offers, and makes that the device
    /// table: the board's probe if there is a board, then the host's.
    /// Called when `/sys` and `/dev` are built, which is at every `execve`
    /// — so a program finds the devices there were when it started, as one
    /// does that lists them once.
    pub(crate) fn enumerate_usb(&mut self) {
        self.look_for_board_now();
        let mut ids = Vec::new();
        if matches!(&*self.board.borrow(), crate::board::Slot::Present(_)) {
            ids.push(BOARD.to_string());
        }
        let mut listed = Vec::new();
        let mut path: Vec<&[u8]> = crate::paths::USB.to_vec();
        path.push(b"devices");
        if self.store.read(&path, &mut listed) == StoreOutcome::Present {
            ids.extend(String::from_utf8_lossy(&listed).lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from));
        }
        let mut devices = Vec::new();
        for id in ids {
            let mut descriptors = Vec::new();
            if self.usb_read(&id, &[b"descriptors"], &mut descriptors) != StoreOutcome::Present || descriptors.len() < 18 {
                continue;
            }
            let mut device = Device {
                id,
                address: devices.len() as u8 + 2,
                descriptors,
                strings: Vec::new(),
                speed: String::new(),
            };
            for index in device.string_indices() {
                let mut text = Vec::new();
                let number = index.to_string();
                if self.usb_read(&device.id.clone(), &[b"strings", number.as_bytes()], &mut text) == StoreOutcome::Present {
                    device.strings.push((index, String::from_utf8_lossy(&text).into_owned()));
                }
            }
            let mut speed = Vec::new();
            device.speed = match self.usb_read(&device.id.clone(), &[b"speed"], &mut speed) {
                StoreOutcome::Present => String::from_utf8_lossy(&speed).trim().to_string(),
                _ => "12".to_string(),
            };
            devices.push(device);
        }
        let mut usb = self.usb.borrow_mut();
        // A device that went keeps its handles pointing nowhere rather than
        // at whichever device took its place.
        if usb.devices.iter().map(|d| &d.id).ne(devices.iter().map(|d| &d.id)) {
            for handle in usb.handles.iter_mut().flatten() {
                handle.device = usize::MAX;
            }
        }
        usb.devices = devices;
    }

    /// Opens `/dev/bus/usb/001/{minor + 1}`.
    pub(crate) fn open_usb(&mut self, minor: u32, flags: i32) -> Result<i32, Errno> {
        let device = self.usb.borrow().by_minor(minor).ok_or(Errno::NoDevice)?;
        let id = self.usb.borrow_mut().open(device);
        let close_on_exec = flags & crate::file::open_flags::CLOEXEC != 0;
        self.files.open(crate::fd::Backing::Usb(id), flags & !crate::file::open_flags::CLOEXEC, close_on_exec).inspect_err(|_| {
            self.usb.borrow_mut().release(id);
        })
    }

    /// `read` on a device file: its descriptors, as Linux keeps them.
    pub(crate) fn read_usb(&mut self, id: u32, buffer: u64, count: u64, offset: u64) -> Result<u64, Errno> {
        let descriptors = {
            let usb = self.usb.borrow();
            let handle = usb.handle(id).ok_or(Errno::BadFile)?;
            usb.devices.get(handle.device).ok_or(Errno::NoDevice)?.descriptors.clone()
        };
        let from = (offset as usize).min(descriptors.len());
        let to = from.saturating_add(count as usize).min(descriptors.len());
        self.pages.write(buffer, &descriptors[from..to]).map_err(|_| Errno::Fault)?;
        Ok((to - from) as u64)
    }

    fn handle_device(&self, id: u32) -> Result<Device, Errno> {
        let usb = self.usb.borrow();
        let handle = usb.handle(id).ok_or(Errno::BadFile)?;
        usb.devices.get(handle.device).cloned().ok_or(Errno::NoDevice)
    }

    /// One `usbfs` ioctl on handle `id`.
    pub(crate) fn usb_ioctl(&mut self, id: u32, arguments: Arguments) -> Outcome {
        let request = arguments.get(1) as u64;
        let at = arguments.get(2) as u64;
        Outcome::Done(match self.usb_request(id, request, at) {
            Ok(value) => value,
            Err(errno) => errno.as_result(),
        })
    }

    fn usb_request(&mut self, id: u32, request: u64, at: u64) -> Result<i64, Errno> {
        let device = self.handle_device(id)?;
        match request {
            ioctl::SUBMITURB => self.submit_urb(id, &device, at).map(|()| 0),
            ioctl::REAPURBNDELAY => self.reap_urb(id, at),
            // A blocking reap: nothing found to need it, since `epoll` is how
            // nusb and libusb both wait. Refused by name rather than spun.
            ioctl::REAPURB => {
                let reapable = self.usb.borrow().reapable(id);
                match reapable {
                    true => self.reap_urb(id, at),
                    false => Err(Errno::NoSys),
                }
            }
            ioctl::DISCARDURB => {
                let mut usb = self.usb.borrow_mut();
                let handle = usb.handle_mut(id).ok_or(Errno::BadFile)?;
                let index = handle.pending.iter().position(|u| u.user == at).ok_or(Errno::Invalid)?;
                let mut urb = handle.pending.remove(index);
                urb.status = Errno::NoEntry.as_result() as i32;
                urb.actual = 0;
                handle.completed.push_back(urb);
                Ok(0)
            }
            ioctl::CONTROL => {
                let mut setup = [0u8; 24];
                self.pages.read(at, &mut setup).map_err(|_| Errno::Fault)?;
                let length = u16::from_le_bytes([setup[6], setup[7]]) as usize;
                let data = u64::from_le_bytes(setup[16..24].try_into().unwrap());
                let mut out = vec![0u8; if setup[0] & 0x80 == 0 { length } else { 0 }];
                if !out.is_empty() {
                    self.pages.read(data, &mut out).map_err(|_| Errno::Fault)?;
                }
                let reply = self.control(&device, setup[..8].try_into().unwrap(), &out)?;
                if setup[0] & 0x80 != 0 {
                    let reply = &reply[..reply.len().min(length)];
                    self.pages.write(data, reply).map_err(|_| Errno::Fault)?;
                    Ok(reply.len() as i64)
                } else {
                    Ok(length as i64)
                }
            }
            ioctl::CLAIMINTERFACE
            | ioctl::RELEASEINTERFACE
            | ioctl::SETINTERFACE
            | ioctl::SETCONFIGURATION
            | ioctl::CLEAR_HALT
            | ioctl::RESET
            | ioctl::DISCONNECT_CLAIM => Ok(0),
            // Connecting or disconnecting a kernel driver: there are none.
            ioctl::IOCTL => Err(Errno::NoData),
            ioctl::GET_CAPABILITIES => {
                self.pages.write(at, &CAPABILITIES.to_le_bytes()).map_err(|_| Errno::Fault)?;
                Ok(0)
            }
            ioctl::GET_SPEED => Ok(SPEED_HIGH),
            ioctl::CONNECTINFO => {
                let mut info = [0u8; 8];
                info[0..4].copy_from_slice(&u32::from(device.address).to_le_bytes());
                self.pages.write(at, &info).map_err(|_| Errno::Fault)?;
                Ok(0)
            }
            _ => Err(Errno::NoTty),
        }
    }

    /// A control request: the standard ones from the descriptors, the rest
    /// from the provider. A stall is `EPIPE`.
    fn control(&mut self, device: &Device, setup: [u8; 8], out: &[u8]) -> Result<Vec<u8>, Errno> {
        let value = u16::from_le_bytes([setup[2], setup[3]]);
        if setup[0] & 0x60 == 0 {
            return match setup[1] {
                // GET_STATUS.
                0x00 => Ok(vec![0, 0]),
                // GET_DESCRIPTOR: device, configuration, string.
                0x06 => match value >> 8 {
                    1 => Ok(device.descriptors[..18].to_vec()),
                    2 => Ok(device.rest().to_vec()),
                    3 => device.string_descriptor(value as u8).ok_or(Errno::Pipe),
                    _ => Err(Errno::Pipe),
                },
                // GET_CONFIGURATION.
                0x08 => Ok(vec![device.rest().get(5).copied().unwrap_or(1)]),
                // GET_INTERFACE.
                0x0A => Ok(vec![0]),
                // CLEAR_FEATURE, SET_FEATURE, SET_CONFIGURATION, SET_INTERFACE.
                0x01 | 0x03 | 0x09 | 0x0B => Ok(Vec::new()),
                _ => Err(Errno::Pipe),
            };
        }
        let mut request = setup.to_vec();
        request.extend_from_slice(out);
        if self.usb_write(&device.id, &[b"control"], &request) != StoreOutcome::Present {
            return Err(Errno::Pipe);
        }
        let mut reply = Vec::new();
        match self.usb_read(&device.id, &[b"control"], &mut reply) {
            StoreOutcome::Failed => Err(Errno::Pipe),
            _ => Ok(reply),
        }
    }

    fn submit_urb(&mut self, id: u32, device: &Device, at: u64) -> Result<(), Errno> {
        let mut raw = [0u8; URB];
        self.pages.read(at, &mut raw).map_err(|_| Errno::Fault)?;
        let kind = raw[0];
        let endpoint = raw[1];
        let buffer = u64::from_le_bytes(raw[16..24].try_into().unwrap());
        let length = i32::from_le_bytes(raw[24..28].try_into().unwrap());
        let length = u32::try_from(length).map_err(|_| Errno::Invalid)?;
        let urb = |into: u64| Urb { user: at, endpoint, into, length, status: 0, actual: 0, data: Vec::new() };
        match kind {
            TYPE_CONTROL => {
                if length < 8 {
                    return Err(Errno::Invalid);
                }
                let mut setup = [0u8; 8];
                self.pages.read(buffer, &mut setup).map_err(|_| Errno::Fault)?;
                let wanted = u16::from_le_bytes([setup[6], setup[7]]) as u32;
                let mut out = vec![0u8; if setup[0] & 0x80 == 0 { wanted.min(length - 8) as usize } else { 0 }];
                if !out.is_empty() {
                    self.pages.read(buffer + 8, &mut out).map_err(|_| Errno::Fault)?;
                }
                let mut urb = urb(buffer + 8);
                urb.length = (length - 8).min(wanted);
                match self.control(device, setup, &out) {
                    Ok(reply) if setup[0] & 0x80 != 0 => {
                        urb.actual = reply.len().min(urb.length as usize) as u32;
                        urb.data = reply;
                    }
                    Ok(_) => urb.actual = out.len() as u32,
                    Err(errno) => urb.status = errno.as_result() as i32,
                }
                let mut usb = self.usb.borrow_mut();
                usb.handle_mut(id).ok_or(Errno::BadFile)?.completed.push_back(urb);
            }
            TYPE_BULK | TYPE_INTERRUPT if endpoint & 0x80 == 0 => {
                let mut bytes = vec![0u8; length as usize];
                self.pages.read(buffer, &mut bytes).map_err(|_| Errno::Fault)?;
                let mut urb = urb(buffer);
                match self.usb_write(&device.id, &[b"endpoints", &endpoint_name(endpoint), b"tx"], &bytes) {
                    StoreOutcome::Failed => urb.status = Errno::Pipe.as_result() as i32,
                    _ => urb.actual = length,
                }
                let at = self.instant();
                let mut usb = self.usb.borrow_mut();
                let handle = usb.handle_mut(id).ok_or(Errno::BadFile)?;
                let transfer = Transfer { at, endpoint, length: urb.actual, status: urb.status, head: bytes[..bytes.len().min(HEAD)].to_vec() };
                handle.completed.push_back(urb);
                let index = handle.device;
                usb.note(index, transfer);
            }
            TYPE_BULK | TYPE_INTERRUPT => {
                self.usb.borrow_mut().handle_mut(id).ok_or(Errno::BadFile)?.pending.push(urb(buffer));
                self.service_usb();
            }
            // Isochronous: nothing offers it.
            _ => return Err(Errno::Invalid),
        }
        Ok(())
    }

    fn reap_urb(&mut self, id: u32, at: u64) -> Result<i64, Errno> {
        if !self.usb.borrow().reapable(id) {
            self.service_usb();
        }
        let urb = {
            let mut usb = self.usb.borrow_mut();
            let handle = usb.handle_mut(id).ok_or(Errno::BadFile)?;
            handle.completed.pop_front().ok_or(Errno::TryAgain)?
        };
        let actual = urb.actual as usize;
        if actual > 0 && !urb.data.is_empty() {
            self.pages.write(urb.into, &urb.data[..actual.min(urb.data.len())]).map_err(|_| Errno::Fault)?;
        }
        self.pages.write(urb.user + URB_STATUS, &urb.status.to_le_bytes()).map_err(|_| Errno::Fault)?;
        self.pages.write(urb.user + URB_ACTUAL, &(urb.actual as i32).to_le_bytes()).map_err(|_| Errno::Fault)?;
        self.pages.write(at, &urb.user.to_le_bytes()).map_err(|_| Errno::Fault)?;
        Ok(0)
    }

    /// Asks the providers for whatever IN transfers have arrived, and
    /// completes the URBs waiting for them. Kernel state and store reads
    /// only, never a guest's memory, so it runs from any process's turn:
    /// wherever a URB is submitted or reaped, and wherever the board is
    /// moved on. Answers whether any completed — whether a parked `epoll`
    /// may now have an answer.
    pub(crate) fn service_usb(&mut self) -> bool {
        let waiting = self.usb.borrow().waiting();
        if waiting.is_empty() {
            return false;
        }
        let at = self.instant();
        let mut moved = false;
        for (handle, device, endpoint, length) in waiting {
            let mut data = Vec::new();
            let max = length.to_string();
            match self.usb_read(&device, &[b"endpoints", &endpoint_name(endpoint), b"pending", max.as_bytes()], &mut data) {
                StoreOutcome::Present => {
                    self.usb.borrow_mut().complete(handle, endpoint, 0, data, at);
                    moved = true;
                }
                StoreOutcome::Failed => {
                    self.usb.borrow_mut().complete(handle, endpoint, Errno::Pipe.as_result() as i32, Vec::new(), at);
                    moved = true;
                }
                StoreOutcome::Absent => {}
            }
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> Device {
        let descriptors = armv6m::cmsis_dap::descriptors();
        let mut device = Device { id: BOARD.into(), address: 2, descriptors, strings: Vec::new(), speed: "480".into() };
        for index in device.string_indices() {
            let text = match index {
                4 => armv6m::cmsis_dap::INTERFACE_STRING,
                i => armv6m::cmsis_dap::STRINGS[i as usize],
            };
            device.strings.push((index, text.into()));
        }
        device
    }

    #[test]
    fn sysfs_says_what_nusb_reads_to_find_a_cmsis_dap_probe() {
        let usb = Usb { devices: vec![probe()], ..Usb::default() };
        let a = |name| usb.attribute(0, None, name).unwrap();
        assert_eq!(a("idVendor"), "1209");
        assert_eq!(a("idProduct"), "0001");
        assert_eq!(a("version"), " 2.00");
        assert_eq!(a("product"), "zaqaru CMSIS-DAP");
        assert_eq!(a("devnum"), "2");
        let i = |name| usb.attribute(0, Some(0), name).unwrap();
        assert_eq!(i("bInterfaceClass"), "ff");
        assert_eq!(i("bInterfaceNumber"), "00");
        assert!(i("interface").contains("CMSIS-DAP"));
    }

    #[test]
    fn a_string_descriptor_is_utf16() {
        let d = probe();
        assert_eq!(d.string_descriptor(0).unwrap(), [4, 3, 9, 4]);
        let s = d.string_descriptor(2).unwrap();
        assert_eq!(s[0] as usize, s.len());
        assert_eq!(&s[2..6], &[b'z', 0, b'a', 0]);
    }
}
