//! One file out of a zip archive: enough to read what a Hubris build
//! archive says about itself — `img/flash.ron`, the part it is for —
//! without running a program to ask. Hubris writes its archives with
//! bzip2, so that is read too, beside the stored and deflated entries
//! every other zip has.
//!
//! And an archive repacked ([`repack`]): Humility reads the whole of one
//! for every command, and bzip2 is the slowest of the three to read —
//! under zaqaru's interpreter about half of what a command costs — so the
//! bench keeps its archives deflated, or stored, instead.

/// The bytes of the entry named `name`, stored, deflated or bzip2; `None` if the
/// archive has no such entry or is not one this reads.
pub fn entry(archive: &[u8], name: &str) -> Option<Vec<u8>> {
    let directory = Directory::find(archive)?;
    let mut at = directory.start;
    for _ in 0..directory.count {
        let header = Header::at(archive, at)?;
        if header.name == name.as_bytes() {
            return header.contents(archive);
        }
        at = header.next;
    }
    None
}

/// The names of the archive's entries, in its directory's order; empty if
/// it is not an archive this reads.
pub fn names(archive: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let Some(directory) = Directory::find(archive) else { return out };
    let mut at = directory.start;
    for _ in 0..directory.count {
        let Some(header) = Header::at(archive, at) else { break };
        out.push(String::from_utf8_lossy(header.name).into_owned());
        at = header.next;
    }
    out
}

/// How [`repack`] writes the entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    Stored,
    /// Deflated, at a level from 1 (fastest) to 10.
    Deflated(u8),
}

/// The same archive with every entry stored or deflated afresh: the same
/// names, contents, checksums, dates and attributes, and the same comment
/// — where a Hubris archive says its version. `None` if it is not an
/// archive this reads.
pub fn repack(archive: &[u8], packing: Packing) -> Option<Vec<u8>> {
    let directory = Directory::find(archive)?;
    let (mut out, mut central) = (Vec::new(), Vec::new());
    let mut at = directory.start;
    for _ in 0..directory.count {
        let header = Header::at(archive, at)?;
        let contents = header.contents(archive)?;
        let (method, data) = match packing {
            Packing::Stored => (0u16, contents),
            Packing::Deflated(level) => (8, miniz_oxide::deflate::compress_to_vec(&contents, level)),
        };
        let local = out.len() as u32;
        // What the local and central headers share: version needed, no
        // flags, the method, the date, the checksum and the sizes.
        let mut shared = vec![20, 0, 0, 0];
        shared.extend(method.to_le_bytes());
        shared.extend(&header.raw[12..20]);
        shared.extend((data.len() as u32).to_le_bytes());
        shared.extend(&header.raw[24..28]);
        shared.extend((header.name.len() as u16).to_le_bytes());
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(&shared);
        out.extend([0, 0]);
        out.extend(header.name);
        out.extend(&data);
        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(&header.raw[4..6]);
        central.extend(&shared);
        // No extra field or comment; the disk, and the attributes as they
        // were.
        central.extend([0, 0, 0, 0, 0, 0]);
        central.extend(&header.raw[36..42]);
        central.extend(local.to_le_bytes());
        central.extend(header.name);
        at = header.next;
    }
    let (start, size) = (out.len() as u32, central.len() as u32);
    out.extend(central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0; 4]);
    out.extend((directory.count as u16).to_le_bytes());
    out.extend((directory.count as u16).to_le_bytes());
    out.extend(size.to_le_bytes());
    out.extend(start.to_le_bytes());
    out.extend((directory.comment.len() as u16).to_le_bytes());
    out.extend(directory.comment);
    Some(out)
}

/// Whether any entry is compressed with bzip2.
pub fn has_bzip2(archive: &[u8]) -> bool {
    let Some(directory) = Directory::find(archive) else { return false };
    let mut at = directory.start;
    for _ in 0..directory.count {
        let Some(header) = Header::at(archive, at) else { return false };
        if header.method == 12 {
            return true;
        }
        at = header.next;
    }
    false
}

fn u16_at(archive: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(archive.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn u32_at(archive: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(archive.get(at..at + 4)?.try_into().ok()?) as usize)
}

/// The end-of-central-directory record: where the directory starts, how
/// many entries it has, and the archive's comment.
struct Directory<'a> {
    start: usize,
    count: usize,
    comment: &'a [u8],
}

impl<'a> Directory<'a> {
    /// Searched for backwards past a comment of up to 64 KiB.
    fn find(archive: &'a [u8]) -> Option<Directory<'a>> {
        let end = (archive.len().saturating_sub(22 + 0xFFFF)..=archive.len().checked_sub(22)?)
            .rev()
            .find(|&at| u32_at(archive, at) == Some(0x0605_4b50))?;
        let comment = u16_at(archive, end + 20)?;
        Some(Directory {
            start: u32_at(archive, end + 16)?,
            count: u16_at(archive, end + 10)?,
            comment: archive.get(end + 22..end + 22 + comment)?,
        })
    }
}

/// An entry's central directory header.
struct Header<'a> {
    /// The fixed part, as it is.
    raw: &'a [u8],
    name: &'a [u8],
    method: usize,
    /// Where its data starts, and how long it is.
    data: usize,
    compressed: usize,
    /// Where the next header starts.
    next: usize,
}

impl<'a> Header<'a> {
    fn at(archive: &'a [u8], at: usize) -> Option<Header<'a>> {
        if u32_at(archive, at)? != 0x0201_4b50 {
            return None;
        }
        let name_length = u16_at(archive, at + 28)?;
        let extra = u16_at(archive, at + 30)?;
        let comment = u16_at(archive, at + 32)?;
        let local = u32_at(archive, at + 42)?;
        // The local header repeats the name and has its own extra field,
        // whose length may differ from the central one's.
        let data = local + 30 + u16_at(archive, local + 26)? + u16_at(archive, local + 28)?;
        Some(Header {
            raw: archive.get(at..at + 46)?,
            name: archive.get(at + 46..at + 46 + name_length)?,
            method: u16_at(archive, at + 10)?,
            data,
            compressed: u32_at(archive, at + 20)?,
            next: at + 46 + name_length + extra + comment,
        })
    }

    fn contents(&self, archive: &[u8]) -> Option<Vec<u8>> {
        let bytes = archive.get(self.data..self.data + self.compressed)?;
        match self.method {
            0 => Some(bytes.to_vec()),
            8 => miniz_oxide::inflate::decompress_to_vec(bytes).ok(),
            12 => {
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut bzip2_rs::DecoderReader::new(bytes), &mut out).ok()?;
                Some(out)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Packing, entry, has_bzip2, repack};

    /// A zip of `(name, method, stored bytes, original length)` entries.
    fn archive(entries: &[(&str, u16, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, method, data) in entries {
            let local = out.len() as u32;
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend([20, 0, 0, 0]);
            out.extend(method.to_le_bytes());
            out.extend([0; 8]);
            out.extend((data.len() as u32).to_le_bytes());
            out.extend([0; 4]);
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(3u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend([1, 2, 3]);
            out.extend(data);
            central.extend(0x0201_4b50u32.to_le_bytes());
            central.extend([20, 0, 20, 0, 0, 0]);
            central.extend(method.to_le_bytes());
            central.extend([0; 8]);
            central.extend((data.len() as u32).to_le_bytes());
            central.extend([0; 4]);
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0; 12]);
            central.extend(local.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let start = out.len() as u32;
        let size = central.len() as u32;
        out.extend(central);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0; 4]);
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(start.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    #[test]
    fn a_stored_or_deflated_entry_reads_back() {
        let ron = b"(\n    chip: Some(\"STM32G070RBTx\"),\n)\n".to_vec();
        let zip = archive(&[
            ("app.toml", 0, b"name = \"x\"".to_vec()),
            ("img/flash.ron", 8, miniz_oxide::deflate::compress_to_vec(&ron, 6)),
        ]);
        assert_eq!(entry(&zip, "img/flash.ron"), Some(ron));
        assert_eq!(entry(&zip, "app.toml").as_deref(), Some(&b"name = \"x\""[..]));
        assert_eq!(entry(&zip, "missing"), None);
        assert_eq!(entry(b"not a zip", "img/flash.ron"), None);
    }

    /// What a Hubris archive holds: entries compressed with bzip2.
    #[test]
    fn a_bzip2_entry_reads_back() {
        // `bz2.compress` of the flash.ron below.
        let compressed = vec![66, 90, 104, 57, 49, 65, 89, 38, 83, 89, 29, 197, 126, 206, 0, 0, 5, 223, 128, 64, 16, 80, 100, 88, 144, 16, 138, 12, 0, 10, 98, 192, 64, 32, 0, 49, 64, 0, 1, 147, 32, 198, 104, 212, 109, 19, 210, 99, 212, 39, 168, 248, 68, 203, 152, 205, 140, 131, 165, 221, 85, 108, 173, 123, 16, 79, 64, 31, 107, 108, 108, 159, 139, 185, 34, 156, 40, 72, 14, 226, 191, 103, 0];
        let zip = archive(&[("img/flash.ron", 12, compressed)]);
        assert_eq!(
            entry(&zip, "img/flash.ron").as_deref(),
            Some(&b"(\n    chip: Some(\"STM32G070KBTx\"),\n)\n"[..])
        );
        assert!(has_bzip2(&zip));
        for packing in [Packing::Stored, Packing::Deflated(9)] {
            let repacked = repack(&zip, packing).unwrap();
            assert!(!has_bzip2(&repacked));
            assert_eq!(entry(&repacked, "img/flash.ron"), entry(&zip, "img/flash.ron"));
        }
    }

    /// A repacked archive is one Python's zipfile, and so Humility's zip
    /// crate, reads: checked on the bench's own archive when it is there.
    #[test]
    fn the_bench_s_archive_repacks_with_its_comment() {
        let Ok(archive) = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/demo/hubris-build/bench/hubris.zip"))
        else {
            return;
        };
        let repacked = repack(&archive, Packing::Deflated(6)).unwrap();
        assert!(repacked.ends_with(b"hubris build archive v13") || repacked.windows(13).any(|w| w == b"hubris build "));
        assert_eq!(entry(&repacked, "img/final.bin"), entry(&archive, "img/final.bin"));
    }
}
