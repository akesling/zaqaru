//! One file out of a zip archive: enough to read what a Hubris build
//! archive says about itself — `img/flash.ron`, the part it is for —
//! without running a program to ask.

/// The bytes of the entry named `name`, stored or deflated; `None` if the
/// archive has no such entry or is not one this reads.
pub fn entry(archive: &[u8], name: &str) -> Option<Vec<u8>> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(archive.get(at..at + 2)?.try_into().ok()?) as usize);
    let u32_at = |at: usize| Some(u32::from_le_bytes(archive.get(at..at + 4)?.try_into().ok()?) as usize);
    // The end-of-central-directory record, searched for backwards past a
    // comment of up to 64 KiB.
    let end = (archive.len().saturating_sub(22 + 0xFFFF)..=archive.len().checked_sub(22)?)
        .rev()
        .find(|&at| u32_at(at) == Some(0x0605_4b50))?;
    let count = u16_at(end + 10)?;
    let mut at = u32_at(end + 16)?;
    for _ in 0..count {
        if u32_at(at)? != 0x0201_4b50 {
            return None;
        }
        let method = u16_at(at + 10)?;
        let compressed = u32_at(at + 20)?;
        let name_length = u16_at(at + 28)?;
        let extra = u16_at(at + 30)?;
        let comment = u16_at(at + 32)?;
        let local = u32_at(at + 42)?;
        let named = archive.get(at + 46..at + 46 + name_length)?;
        if named == name.as_bytes() {
            // The local header repeats the name and has its own extra field,
            // whose length may differ from the central one's.
            let data = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
            let bytes = archive.get(data..data + compressed)?;
            return match method {
                0 => Some(bytes.to_vec()),
                8 => miniz_oxide::inflate::decompress_to_vec(bytes).ok(),
                _ => None,
            };
        }
        at += 46 + name_length + extra + comment;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::entry;

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
}
