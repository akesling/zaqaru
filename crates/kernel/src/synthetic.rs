//! The mounts an image does not carry: `/dev` and `/proc`.
//!
//! A base image ships `/dev/null` as a device node with no driver behind it,
//! and `/proc` as an empty directory — because on a real system both are
//! *mounts*, filled in by the kernel at boot. So they are filled in here, the
//! same way: a small filesystem built at boot and attached over the
//! directory the image provides.
//!
//! Built as ordinary images, in the same packed format the packager writes, so
//! resolution, `stat`, `getdents64` and the rest work on them without
//! knowing they are synthetic. Only *reads* differ, and they differ where a
//! real kernel differs too: an inode whose type is a character device sends
//! its reads to a driver instead of to a blob. That dispatch is on the
//! inode's own type and its device number, which is exactly what a real VFS
//! does, rather than on a path that something could have moved.
//!
//! The format is written with `kernel::image`'s own writers — the same
//! `Inode::write` and `write_header` the packager calls — so there is no second
//! definition of the layout to drift from the first.

use std::vec;
use std::vec::Vec;

use crate::image::{
    HEADER_SIZE, INODE_SIZE, Image, ImageError, Inode, directory_entry_type, file_type, inode_flags,
};

/// The device numbers Linux gives these, which is what a program that looks
/// at `st_rdev` expects to see. `/dev/null` is (1, 3) everywhere.
pub const NULL_DEVICE: (u32, u32) = (1, 3);
pub const ZERO_DEVICE: (u32, u32) = (1, 5);
pub const FULL_DEVICE: (u32, u32) = (1, 7);
pub const RANDOM_DEVICE: (u32, u32) = (1, 8);
pub const URANDOM_DEVICE: (u32, u32) = (1, 9);

/// Which generated view a `GENERATED` inode is. Small and closed: a view
/// nothing renders would be a file that reads as empty, which is a plausible
/// wrong answer.
pub mod view {
    /// `/proc/self/maps`, rendered from the VMA tree.
    pub const MAPS: u64 = 1;
    /// `/proc/board`, the board on the container's bench (see
    /// `crate::board`), rendered from its state.
    pub const BOARD: u64 = 2;
    /// A USB device's sysfs attribute, rendered from the device table (see
    /// `crate::usb`). The low byte says so; above it, [`usb`] packs which
    /// device, interface and attribute.
    pub const USB: u64 = 3;

    pub const fn usb(device: usize, interface: Option<usize>, attribute: usize) -> u64 {
        let interface = match interface {
            Some(n) => n as u64 + 1,
            None => 0,
        };
        USB | (device as u64) << 8 | interface << 24 | (attribute as u64) << 32
    }

    /// The device, interface and attribute a [`usb`] view names.
    pub const fn usb_parts(payload: u64) -> (usize, Option<usize>, usize) {
        let interface = (payload >> 24 & 0xFF) as usize;
        (
            (payload >> 8 & 0xFFFF) as usize,
            if interface == 0 { None } else { Some(interface - 1) },
            (payload >> 32) as usize,
        )
    }
}

/// Linux's packed `dev_t`: minor's low eight bits, major's twelve above
/// them, and minor's remaining twelve above that. Not a plain shift, and the
/// difference is invisible until a minor exceeds 255.
pub const fn make_device(major: u32, minor: u32) -> u64 {
    ((major as u64 & 0xfff) << 8) | (minor as u64 & 0xff) | ((minor as u64 & 0xfff00) << 12)
}

/// The inverse of [`make_device`]: a packed `dev_t`'s major and minor.
pub const fn split_device(rdev: u64) -> (u32, u32) {
    (((rdev >> 8) & 0xfff) as u32, ((rdev & 0xff) | ((rdev >> 12) & 0xfff00)) as u32)
}

/// One device node to build.
struct Entry {
    name: &'static [u8],
    mode: u32,
    /// Linux's packed `rdev`, which is what the read dispatch keys on.
    rdev: u64,
}

/// The console, as three devices.
///
/// A base image ships `/dev/stdout` and friends as symlinks into
/// `/proc/self/fd`, which this kernel does not have — so an image that
/// writes there gets a dangling link and a program that logs there does not
/// start. nginx does exactly that, and `open("/dev/stderr") failed (30:
/// Read-only file system)` is how it says so.
///
/// Character devices instead of symlinks, and the difference is worth
/// stating: a symlink would make `/dev/stdout` mean *this process's*
/// descriptor 1, which is what `/proc/self/fd` means and what a shell
/// redirection changes. These mean the container's console, always. For a
/// container whose stdout is the console that is the same thing, and it is
/// what every image that logs to `/dev/stdout` is asking for.
pub const CONSOLE_INPUT_DEVICE: (u32, u32) = (5, 100);
pub const CONSOLE_OUTPUT_DEVICE: (u32, u32) = (5, 101);
pub const CONSOLE_ERROR_DEVICE: (u32, u32) = (5, 102);

/// A synthetic filesystem as a tree, for the ones too deep for [`build`].
pub enum Node {
    Directory(Vec<(Vec<u8>, Node)>),
    Device { mode: u32, rdev: u64 },
    /// A file rendered when read: a [`view`].
    Generated(u64),
}

/// Builds a tree whose root is a directory of `entries`.
pub fn tree(entries: Vec<(Vec<u8>, Node)>) -> Vec<u8> {
    let mut index = Vec::new();
    let mut inodes: Vec<Inode> = vec![directory(0)];
    let mut strings = Strings::default();
    let mut dirents = Vec::new();
    // Breadth first: each directory's children get their numbers before any
    // grandchild does, and its listing is written when it is reached.
    let mut queue: std::collections::VecDeque<(u32, Vec<(Vec<u8>, Node)>)> = std::collections::VecDeque::new();
    queue.push_back((0, entries));
    while let Some((number, children)) = queue.pop_front() {
        let mut listing: Vec<(Vec<u8>, u32, u8)> = Vec::new();
        let mut subdirectories = 0;
        for (name, node) in children {
            let child = inodes.len() as u32;
            match node {
                Node::Directory(grandchildren) => {
                    inodes.push(directory(number));
                    subdirectories += 1;
                    listing.push((name, child, directory_entry_type::DIRECTORY));
                    queue.push_back((child, grandchildren));
                }
                Node::Device { mode, rdev } => {
                    inodes.push(Inode { mode, payload: rdev, nlink: 1, ..blank() });
                    listing.push((name, child, directory_entry_type::of_mode(mode)));
                }
                Node::Generated(payload) => {
                    inodes.push(Inode {
                        mode: file_type::REGULAR | 0o444,
                        size: 0,
                        payload,
                        flags: inode_flags::GENERATED,
                        nlink: 1,
                        ..blank()
                    });
                    listing.push((name, child, directory_entry_type::REGULAR));
                }
            }
        }
        let borrowed: Vec<(&[u8], u32, u8)> = listing.iter().map(|(n, i, k)| (n.as_slice(), *i, *k)).collect();
        let at = dirents.len() as u64;
        write_entries(&mut dirents, &mut strings, &borrowed);
        let inode = &mut inodes[number as usize];
        inode.payload = at;
        inode.size = (4 + 12 * listing.len()) as u64;
        inode.nlink = 2 + subdirectories;
    }
    assemble(&mut index, &inodes, &dirents, &strings.bytes);
    index
}

/// `/sys`: as much of Linux's as finds USB devices — `bus/usb/devices`,
/// a directory per device with its attributes, and inside it a directory
/// per interface with theirs, which is where libusb and nusb look — and
/// the device classes probe-rs walks besides.
pub fn sys(usb: &crate::usb::Usb) -> Vec<u8> {
    use crate::usb::{DEVICE_ATTRIBUTES, INTERFACE_ATTRIBUTES};
    let mut devices = Vec::new();
    for (n, device) in usb.devices.iter().enumerate() {
        let name = format!("{}-{}", crate::usb::BUS, device.address - 1);
        let mut entries: Vec<(Vec<u8>, Node)> = DEVICE_ATTRIBUTES
            .iter()
            .enumerate()
            .filter(|(_, a)| usb.attribute(n, None, a).is_some())
            .map(|(k, a)| (a.as_bytes().to_vec(), Node::Generated(view::usb(n, None, k))))
            .collect();
        for i in 0..device.interfaces().len() {
            let attributes = INTERFACE_ATTRIBUTES
                .iter()
                .enumerate()
                .filter(|(_, a)| usb.attribute(n, Some(i), a).is_some())
                .map(|(k, a)| (a.as_bytes().to_vec(), Node::Generated(view::usb(n, Some(i), k))))
                .collect();
            entries.push((format!("{name}:1.{i}").into_bytes(), Node::Directory(attributes)));
        }
        devices.push((name.into_bytes(), Node::Directory(entries)));
    }
    let usb = Node::Directory(vec![(b"devices".to_vec(), Node::Directory(devices))]);
    // And the classes a device search walks on its way, present and empty:
    // there are no terminals or HID devices here, and a directory that is
    // missing rather than empty is a system with no sysfs at all — which
    // serialport, inside probe-rs, panics on.
    let class = Node::Directory(vec![
        (b"hidraw".to_vec(), Node::Directory(Vec::new())),
        (b"tty".to_vec(), Node::Directory(Vec::new())),
    ]);
    tree(vec![
        (b"bus".to_vec(), Node::Directory(vec![(b"usb".to_vec(), usb)])),
        (b"class".to_vec(), class),
    ])
}

/// `/dev`, with the character devices a libc actually opens.
///
/// Not the whole of a real `/dev`: no `tty`, because v0 decides stdio is not
/// a terminal and a `/dev/tty` that answered would contradict that. What is
/// here is what a process opens before it does anything else, plus the three
/// console devices above.
pub fn dev(usb: &[u8]) -> Vec<u8> {
    let flat = [
        Entry {
            name: b"null",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(NULL_DEVICE.0, NULL_DEVICE.1),
        },
        Entry {
            name: b"stdin",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(CONSOLE_INPUT_DEVICE.0, CONSOLE_INPUT_DEVICE.1),
        },
        Entry {
            name: b"stdout",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(CONSOLE_OUTPUT_DEVICE.0, CONSOLE_OUTPUT_DEVICE.1),
        },
        Entry {
            name: b"stderr",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(CONSOLE_ERROR_DEVICE.0, CONSOLE_ERROR_DEVICE.1),
        },
        Entry {
            name: b"zero",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(ZERO_DEVICE.0, ZERO_DEVICE.1),
        },
        Entry {
            name: b"full",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(FULL_DEVICE.0, FULL_DEVICE.1),
        },
        // Both, and both are the same stream. Linux stopped distinguishing
        // them in 5.6 — `/dev/random` no longer blocks once the pool is
        // initialised — and a container that made `/dev/random` block would
        // hang programs that still prefer it out of habit.
        Entry {
            name: b"random",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(RANDOM_DEVICE.0, RANDOM_DEVICE.1),
        },
        Entry {
            name: b"urandom",
            mode: file_type::CHARACTER | 0o666,
            rdev: make_device(URANDOM_DEVICE.0, URANDOM_DEVICE.1),
        },
    ];
    if usb.is_empty() {
        return build(&flat);
    }
    // With USB devices, `bus/usb/001/{address}` as well, which is two levels
    // deeper than the rest.
    let mut entries: Vec<(Vec<u8>, Node)> =
        flat.iter().map(|e| (e.name.to_vec(), Node::Device { mode: e.mode, rdev: e.rdev })).collect();
    let nodes = usb
        .iter()
        .map(|&address| {
            let node = Node::Device {
                mode: file_type::CHARACTER | 0o666,
                rdev: make_device(crate::usb::MAJOR, u32::from(address) - 1),
            };
            (format!("{address:03}").into_bytes(), node)
        })
        .collect();
    let bus = Node::Directory(vec![(format!("{:03}", crate::usb::BUS).into_bytes(), Node::Directory(nodes))]);
    entries.push((b"bus".to_vec(), Node::Directory(vec![(b"usb".to_vec(), bus)])));
    tree(entries)
}

/// `/proc`: `self/exe`, `self/maps`, `board`, and nothing else.
///
/// `self/exe` and `self/maps`, and nothing else yet of Linux's; `board` is
/// this kernel's own, and reads `ENODEV` in a container without one. What is absent is
/// absent: `stat("/proc/self/status")` is `ENOENT`, which is what a
/// filesystem says about a name it does not have.
///
/// That is worth being uncomfortable about rather than papering over. A real
/// procfs has no such gaps, so a program that probes for one of these and
/// gets `ENOENT` may conclude there is no procfs at all and take a branch
/// nobody tested. The alternative — a loud fault on every absent name —
/// would kill a container for probing an optional file, which programs do
/// constantly. The files get added as something is found to need them; this
/// comment is here so that the next person to hit it knows the gap is known
/// rather than overlooked.
pub fn proc(executable: &[u8]) -> Vec<u8> {
    // `self` is a directory holding `exe`. Built by hand rather than by the
    // generic builder because it is two levels deep.
    let mut index = Vec::new();
    let mut inodes: Vec<Inode> = Vec::new();
    let mut strings = Strings::default();

    // 0: the root, 1: `self`, 2: `exe`, 3: `maps`, 4: `board`.
    inodes.push(directory(0));
    inodes.push(directory(0));
    inodes.push(Inode {
        mode: file_type::SYMLINK | 0o777,
        size: executable.len() as u64,
        payload: strings.intern(executable) as u64,
        nlink: 1,
        ..blank()
    });
    // Size zero, as every procfs file reports: its length is not known
    // until it is read, and a program that trusted a size here would
    // allocate the wrong buffer.
    inodes.push(Inode {
        mode: file_type::REGULAR | 0o444,
        size: 0,
        payload: view::MAPS,
        flags: inode_flags::GENERATED,
        nlink: 1,
        ..blank()
    });
    inodes.push(Inode {
        mode: file_type::REGULAR | 0o444,
        size: 0,
        payload: view::BOARD,
        flags: inode_flags::GENERATED,
        nlink: 1,
        ..blank()
    });

    let mut dirents = Vec::new();
    // The root's array, then `self`'s.
    let root_payload = dirents.len() as u64;
    write_entries(
        &mut dirents,
        &mut strings,
        &[
            (b"self", 1, directory_entry_type::DIRECTORY),
            (b"board", 4, directory_entry_type::REGULAR),
        ],
    );
    let self_payload = dirents.len() as u64;
    write_entries(
        &mut dirents,
        &mut strings,
        &[
            (b"exe", 2, directory_entry_type::SYMLINK),
            (b"maps", 3, directory_entry_type::REGULAR),
        ],
    );
    inodes[0].payload = root_payload;
    inodes[0].size = (4 + 12 * 2) as u64;
    inodes[0].nlink = 3; // `.`, `..`, and `self`
    inodes[1].payload = self_payload;
    inodes[1].size = (4 + 12 * 2) as u64;
    inodes[1].parent = 0;

    assemble(&mut index, &inodes, &dirents, &strings.bytes);
    index
}

/// Builds a one-level directory of entries.
fn build(entries: &[Entry]) -> Vec<u8> {
    let mut index = Vec::new();
    let mut inodes: Vec<Inode> = vec![directory(0)];
    let mut strings = Strings::default();
    let mut listing = Vec::new();

    for entry in entries {
        let number = inodes.len() as u32;
        inodes.push(Inode {
            mode: entry.mode,
            payload: entry.rdev,
            nlink: 1,
            ..blank()
        });
        listing.push((
            entry.name,
            number,
            directory_entry_type::of_mode(entry.mode),
        ));
    }

    let mut dirents = Vec::new();
    write_entries(&mut dirents, &mut strings, &listing);
    inodes[0].payload = 0;
    inodes[0].size = (4 + 12 * entries.len()) as u64;
    inodes[0].nlink = 2;

    assemble(&mut index, &inodes, &dirents, &strings.bytes);
    index
}

fn write_entries(dirents: &mut Vec<u8>, strings: &mut Strings, entries: &[(&[u8], u32, u8)]) {
    dirents.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    // Interned first, so the references are known before any is written and
    // sorted, because lookup is a binary search.
    let mut sorted: Vec<(u32, u32, u8)> = entries
        .iter()
        .map(|(name, inode, kind)| (strings.intern(name), *inode, *kind))
        .collect();
    let order: Vec<usize> = {
        let mut indices: Vec<usize> = (0..entries.len()).collect();
        indices.sort_by_key(|index| entries[*index].0);
        indices
    };
    sorted = order.iter().map(|index| sorted[*index]).collect();
    for (name, inode, kind) in sorted {
        dirents.extend_from_slice(&name.to_le_bytes());
        dirents.extend_from_slice(&inode.to_le_bytes());
        dirents.push(kind);
        dirents.extend_from_slice(&[0; 3]);
    }
}

fn assemble(index: &mut Vec<u8>, inodes: &[Inode], dirents: &[u8], strings: &[u8]) {
    let inode_offset = HEADER_SIZE;
    let dirent_offset = inode_offset + inodes.len() * INODE_SIZE;
    let string_offset = dirent_offset + dirents.len();
    let xattr_offset = string_offset + strings.len();

    index.resize(HEADER_SIZE, 0);
    let header: &mut [u8; HEADER_SIZE] =
        (&mut index[..HEADER_SIZE]).try_into().expect("the header");
    crate::image::write_header(
        header,
        &crate::image::Header {
            inode_count: inodes.len() as u32,
            inode_offset: inode_offset as u32,
            dirent_offset: dirent_offset as u32,
            dirent_size: dirents.len() as u32,
            string_offset: string_offset as u32,
            string_size: strings.len() as u32,
            xattr_offset: xattr_offset as u32,
            root_inode: 0,
            // A synthetic mount holds no file contents and is not what a
            // container boots — so no blob, no command line and no
            // environment. Named fields rather than a run of zeroes,
            // because a run of zeroes says which regions are empty only to
            // whoever counts them.
            ..crate::image::Header::default()
        },
    );
    for inode in inodes {
        let mut record = [0u8; INODE_SIZE];
        inode.write(&mut record);
        index.extend_from_slice(&record);
    }
    index.extend_from_slice(dirents);
    index.extend_from_slice(strings);
}

fn directory(parent: u32) -> Inode {
    Inode {
        mode: file_type::DIRECTORY | 0o755,
        nlink: 2,
        parent,
        ..blank()
    }
}

fn blank() -> Inode {
    Inode {
        mode: 0,
        uid: 0,
        gid: 0,
        nlink: 0,
        size: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        xattr_ref: 0,
        payload: 0,
        uname_ref: 0,
        gname_ref: 0,
        flags: 0,
        parent: 0,
    }
}

#[derive(Default)]
struct Strings {
    bytes: Vec<u8>,
}

impl Strings {
    /// The same rule the packager's interning follows: the empty string sits at
    /// zero, so a zero reference reads as empty rather than as whatever
    /// happened to be written first.
    fn intern(&mut self, bytes: &[u8]) -> u32 {
        if self.bytes.is_empty() {
            self.bytes.extend_from_slice(&0u16.to_le_bytes());
            if bytes.is_empty() {
                return 0;
            }
        }
        let reference = self.bytes.len() as u32;
        self.bytes
            .extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        self.bytes.extend_from_slice(bytes);
        reference
    }
}

/// Parses a built index, for a caller that wants the image back.
pub fn parse(index: &[u8]) -> Result<Image<'_>, ImageError> {
    Image::parse(index, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic filesystem is an ordinary image, so everything that reads
    /// an image reads it — which is the point of building it this way rather
    /// than special-casing a path.
    #[test]
    fn dev_reads_back_as_an_image() {
        let index = dev(&[]);
        let image = parse(&index).expect("the built index parses");
        let root = image.inode(image.root()).expect("root");
        assert!(root.is_directory());
        assert_eq!(image.entry_count(&root).expect("count"), 8);

        for (name, (major, minor)) in [
            (&b"null"[..], NULL_DEVICE),
            (b"zero", ZERO_DEVICE),
            (b"full", FULL_DEVICE),
            (b"random", RANDOM_DEVICE),
            (b"urandom", URANDOM_DEVICE),
            // The console, which a base image ships as symlinks into
            // `/proc/self/fd` and this kernel provides as devices — see the
            // constants' own comment.
            (b"stdin", CONSOLE_INPUT_DEVICE),
            (b"stdout", CONSOLE_OUTPUT_DEVICE),
            (b"stderr", CONSOLE_ERROR_DEVICE),
        ] {
            let entry = image
                .lookup(&root, name)
                .expect("lookup")
                .unwrap_or_else(|| panic!("/dev/{} is missing", String::from_utf8_lossy(name)));
            assert_eq!(entry.entry_type, directory_entry_type::CHARACTER);
            let inode = image.inode(entry.inode).expect("inode");
            assert_eq!(inode.file_type(), file_type::CHARACTER);
            assert_eq!(inode.mode & 0o777, 0o666);
            assert_eq!(inode.payload, make_device(major, minor));
            assert_eq!(inode.nlink, 1);
        }

        // Sorted, because lookup is a binary search: an unsorted array finds
        // some names and not others, which is the worst possible failure.
        let names: Vec<Vec<u8>> = (0..image.entry_count(&root).expect("count"))
            .map(|position| {
                let entry = image.entry(&root, position).expect("entry");
                image.string(entry.name_ref).expect("name").to_vec()
            })
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(image.lookup(&root, b"nothing").expect("lookup").is_none());
    }

    /// Walks `path` from the root of a built image.
    fn walk(image: &Image<'_>, path: &[&[u8]]) -> Option<Inode> {
        let mut inode = image.inode(image.root()).ok()?;
        for name in path {
            let entry = image.lookup(&inode, name).ok()??;
            inode = image.inode(entry.inode).ok()?;
        }
        Some(inode)
    }

    #[test]
    fn usb_devices_appear_in_dev_and_sys_where_libusb_looks() {
        let index = dev(&[2]);
        let image = parse(&index).expect("parse");
        let node = walk(&image, &[b"bus", b"usb", b"001", b"002"]).expect("/dev/bus/usb/001/002");
        assert_eq!(node.file_type(), file_type::CHARACTER);
        assert_eq!(split_device(node.payload), (crate::usb::MAJOR, 1));
        assert!(walk(&image, &[b"null"]).is_some(), "and the rest of /dev");

        let device = crate::usb::Device {
            id: crate::usb::BOARD.into(),
            address: 2,
            descriptors: armv6m::cmsis_dap::descriptors(),
            strings: vec![(2, "zaqaru CMSIS-DAP".into()), (4, "zaqaru CMSIS-DAP v2".into())],
            speed: "480".into(),
        };
        let usb = crate::usb::Usb { devices: vec![device], ..Default::default() };
        let index = sys(&usb);
        let image = parse(&index).expect("parse");
        let vendor = walk(&image, &[b"bus", b"usb", b"devices", b"1-1", b"idVendor"]).expect("idVendor");
        assert_eq!(vendor.flags & inode_flags::GENERATED, inode_flags::GENERATED);
        let (device, interface, attribute) = view::usb_parts(vendor.payload);
        assert_eq!(
            usb.attribute(device, interface, crate::usb::DEVICE_ATTRIBUTES[attribute]).as_deref(),
            Some("1209")
        );
        let name = walk(&image, &[b"bus", b"usb", b"devices", b"1-1", b"1-1:1.0", b"interface"]).expect("interface");
        let (device, interface, attribute) = view::usb_parts(name.payload);
        assert_eq!(interface, Some(0));
        assert_eq!(
            usb.attribute(device, interface, crate::usb::INTERFACE_ATTRIBUTES[attribute]).as_deref(),
            Some("zaqaru CMSIS-DAP v2")
        );
        assert!(walk(&image, &[b"bus", b"usb", b"devices", b"1-1", b"serial"]).is_none(), "no string, no file");
    }

    #[test]
    fn proc_carries_the_executable_path() {
        let index = proc(b"/usr/bin/python3");
        let image = parse(&index).expect("parse");
        let root = image.inode(image.root()).expect("root");
        let entry = image
            .lookup(&root, b"self")
            .expect("lookup")
            .expect("/proc/self");
        assert_eq!(entry.entry_type, directory_entry_type::DIRECTORY);
        let self_dir = image.inode(entry.inode).expect("inode");
        assert!(self_dir.is_directory());
        assert_eq!(self_dir.parent, image.root(), "`..` goes back to /proc");

        let exe = image
            .lookup(&self_dir, b"exe")
            .expect("lookup")
            .expect("/proc/self/exe");
        let exe = image.inode(exe.inode).expect("inode");
        assert!(exe.is_symlink());
        assert_eq!(
            image.symlink_target(&exe).expect("target"),
            b"/usr/bin/python3"
        );
        assert_eq!(exe.size, 16, "a symlink's size is its target's length");
    }

    /// Linux's packed device number, which is not a shift — and a minor
    /// above 255 is where the two stop agreeing.
    #[test]
    fn a_device_number_is_packed_the_way_linux_packs_it() {
        assert_eq!(make_device(1, 3), 0x103);
        assert_eq!(make_device(1, 9), 0x109);
        // `makedev(8, 300)` is 0x10082c, not 0x82c.
        assert_eq!(make_device(8, 300), 0x0010_082c);
        assert_eq!(make_device(0, 0), 0);
    }
}
