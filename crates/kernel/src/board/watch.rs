//! Every write to part of the board's RAM, and who writes what everywhere,
//! as the store serves them: see `armv6m::bus::Watch`. A seek turns a
//! watch on in the container it re-executes from a checkpoint, never in
//! the live one, so what it answers is history: every write to a variable
//! between two instants, or every instruction that wrote each variable.
//!
//! The audit compares what was written with what the archive's DWARF says
//! is there. Each write is put down to the code that made it, or, for a
//! helper such as `memcpy` that writes on its caller's behalf, to its
//! caller; and the code that wrote on past the end of one variable into
//! the next, in one run of words, is said. That is what an overrun looks
//! like from outside, whether or not anything faulted: a buffer written a
//! word too far, a stack grown into the memory below it.

use std::collections::{BTreeMap, HashMap};

use armv6m::bus::{BY_DMA, BY_EXCEPTION, BY_PROBE, Watch};
use armv6m::hubris::Image;

use super::{Board, ReadFile, dwarf::line_json};
use crate::system::server::quoted;

/// What startup code writes, all of a task's RAM at once, is no overrun.
const STARTUP: [&str; 5] = ["_start", "start_kernel", "__pre_init", "Reset", "start_first_task"];

/// The helpers that write on their callers' behalf.
const HELPERS: [&str; 5] = ["memcpy", "memset", "memmove", "__aeabi_mem", "compiler_builtins::mem"];

/// A thing in memory the audit puts writes down to: a typed static, or a
/// stack.
struct Object {
    owner: String,
    name: String,
    address: u32,
    size: u32,
    ty: Option<u32>,
}

impl Board {
    /// `board/watch`: `range BASE LENGTH`, to log every write to it;
    /// `audit`, to count who writes what everywhere; `both BASE LENGTH`;
    /// or `off`. Each replaces what was being watched.
    pub fn watch(&mut self, how: &str) -> Result<String, String> {
        let words: Vec<&str> = how.split_whitespace().collect();
        let number = |text: &str| {
            match text.strip_prefix("0x") {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => text.parse().ok(),
            }
            .ok_or_else(|| format!("{text:?} is not a 32-bit number"))
        };
        let watch = match words.as_slice() {
            ["off"] => None,
            ["audit"] => Some(Watch { sites: Some(HashMap::new()), ..Watch::default() }),
            [verb @ ("range" | "both"), base, length] => Some(Watch {
                base: number(base)?,
                length: number(length)?,
                sites: (*verb == "both").then(HashMap::new),
                ..Watch::default()
            }),
            _ => return Err(format!("board/watch takes `range BASE LENGTH`, `audit`, `both BASE LENGTH` or `off`, not {how:?}")),
        };
        self.machine.bus.watch = watch.map(Box::new);
        Ok(format!(r#"{{"watching":{},"since_ns":{}}}"#, self.machine.bus.watch.is_some(), self.machine.time / 1000))
    }

    /// `board/watched`: every write logged to the range being watched,
    /// oldest first, each its time in the board's nanoseconds, its writer's
    /// number in `writers`, its address, size and value; and each writer's
    /// code and source line, as `board/written` gives them.
    pub fn watched_json(&self, map: Option<&Result<Image, String>>, read: ReadFile) -> String {
        let Some(watch) = &self.machine.bus.watch else { return r#"{"watching":false,"writes":[],"writers":[]}"#.into() };
        let mut writers: Vec<u32> = Vec::new();
        let writes: Vec<String> = watch
            .log
            .iter()
            .map(|w| {
                let index = writers.iter().position(|&x| x == w.writer).unwrap_or_else(|| {
                    writers.push(w.writer);
                    writers.len() - 1
                });
                format!("[{},{index},{},{},{}]", w.time / 1000, w.address, w.size, w.value)
            })
            .collect();
        format!(
            r#"{{"watching":true,"base":{},"length":{},"now_ns":{},"dropped":{},"writes":[{}],"writers":[{}]}}"#,
            watch.base,
            watch.length,
            self.machine.time / 1000,
            watch.dropped,
            writes.join(","),
            writers.iter().map(|&w| self.writer_json(w, map, read)).collect::<Vec<_>>().join(",")
        )
    }

    /// A writer as `board/written` says it: its code and source line, or
    /// what wrote without an instruction.
    pub fn writer_json(&self, writer: u32, map: Option<&Result<Image, String>>, read: ReadFile) -> String {
        match writer {
            BY_PROBE => r#"{"code":"the probe"}"#.into(),
            BY_DMA => r#"{"code":"DMA"}"#.into(),
            BY_EXCEPTION => r#"{"code":"the core, stacking an exception"}"#.into(),
            pc => format!(
                r#"{{"pc":{pc},"code":{},"lines":[{}]}}"#,
                quoted(&self.code_at(pc, map)),
                self.lines_at(pc, map, read).iter().map(line_json).collect::<Vec<_>>().join(",")
            ),
        }
    }

    /// Every typed static in RAM, of the kernel and every task, and every
    /// stack: what the audit puts writes down to, by address.
    fn objects(&self, image: &Image, read: ReadFile) -> Vec<Object> {
        let mut out = Vec::new();
        for owner in std::iter::once(&image.kernel).chain(&image.tasks) {
            if let Some((base, size)) = owner.stack {
                out.push(Object { owner: owner.name.clone(), name: "stack".into(), address: base, size, ty: None });
            }
            let in_ram = |a: u32| owner.sections.iter().any(|s| s.writable && a >= s.base && a - s.base < s.size);
            if let Some(Ok(debug)) = self.debug_of(&owner.name, read).as_deref() {
                for s in debug.statics.iter().filter(|s| s.size > 0 && in_ram(s.address)) {
                    out.push(Object { owner: owner.name.clone(), name: s.name.clone(), address: s.address, size: s.size, ty: Some(s.ty) });
                }
            }
        }
        out.sort_by_key(|o| (o.address, std::cmp::Reverse(o.size)));
        out
    }

    /// `board/audit`: what was written, by whom, since the audit began:
    /// each static and stack with the code that wrote it (how many of its
    /// words, how many times), and the code that wrote on from one into
    /// the next.
    pub fn audit_json(&self, map: Option<&Result<Image, String>>, read: ReadFile) -> String {
        let Some(Watch { sites: Some(sites), .. }) = self.machine.bus.watch.as_deref() else {
            return r#"{"auditing":false}"#.into();
        };
        let Some(Ok(image)) = map else { return r#"{"auditing":true,"error":"no archive says what the memory is"}"#.into() };
        let objects = self.objects(image, read);
        let object_at = |a: u32| {
            let n = objects.partition_point(|o| o.address <= a);
            objects[..n].iter().rev().take(4).position(|o| a - o.address < o.size).map(|back| n - 1 - back)
        };
        // Each write put down to a site: the code that made it, or a
        // helper's caller, with the helper beside it.
        let helper = |pc: u32| {
            let code = self.code_at(pc, map);
            HELPERS.iter().any(|h| code.contains(h))
        };
        let mut helpers: HashMap<u32, bool> = HashMap::new();
        let mut by_site: BTreeMap<(u32, u32), BTreeMap<u32, u32>> = BTreeMap::new();
        for (&(writer, caller, word), &count) in sites {
            let via_helper = writer < BY_EXCEPTION && *helpers.entry(writer).or_insert_with(|| helper(writer));
            let site = if via_helper { (caller & !1, writer) } else { (writer, 0) };
            *by_site.entry(site).or_default().entry(word).or_default() += count;
        }
        let site_list: Vec<(u32, u32)> = by_site.keys().copied().collect();
        let site_index: HashMap<(u32, u32), usize> = site_list.iter().enumerate().map(|(n, &s)| (s, n)).collect();
        // Per object: each site's words and writes.
        let mut per_object: BTreeMap<usize, BTreeMap<usize, (u32, u32)>> = BTreeMap::new();
        let mut unowned: BTreeMap<usize, (u32, u32, u32)> = BTreeMap::new(); // site → (first, words, writes)
        let mut findings = Vec::new();
        for (site, words) in &by_site {
            let n = site_index[site];
            let mut last: Option<(u32, Option<usize>)> = None;
            let code = self.code_at(site.0, map);
            let startup = site.0 >= BY_EXCEPTION || STARTUP.iter().any(|s| code.contains(s));
            for (&word, &count) in words {
                let object = object_at(word);
                match object {
                    Some(o) => {
                        let entry = per_object.entry(o).or_default().entry(n).or_default();
                        entry.0 += 1;
                        entry.1 += count;
                    }
                    None => {
                        let entry = unowned.entry(n).or_insert((word, 0, 0));
                        entry.1 += 1;
                        entry.2 += count;
                    }
                }
                // A run of words on from one object into another.
                if let Some((previous, Some(from))) = last
                    && previous + 4 == word
                    && let Some(into) = object
                    && into != from
                    && !startup
                {
                    findings.push(format!(
                        r#"{{"kind":"overrun","site":{n},"from":{},"into":{},"address":{word}}}"#,
                        self.object_json(&objects[from]),
                        self.object_json(&objects[into])
                    ));
                }
                last = Some((word, object));
            }
        }
        let sites_json: Vec<String> = site_list
            .iter()
            .map(|&(pc, via)| {
                let writer = self.writer_json(pc, map, read);
                if via == 0 { writer } else { format!(r#"{{"via":{},"site":{writer}}}"#, quoted(&self.code_at(via, map))) }
            })
            .collect();
        let objects_json: Vec<String> = per_object
            .iter()
            .map(|(&o, sites)| {
                format!(
                    r#"{{"object":{},"sites":[{}]}}"#,
                    self.object_json(&objects[o]),
                    sites.iter().map(|(n, (words, writes))| format!("[{n},{words},{writes}]")).collect::<Vec<_>>().join(",")
                )
            })
            .collect();
        let unowned_json: Vec<String> =
            unowned.iter().map(|(n, (first, words, writes))| format!("[{n},{first},{words},{writes}]")).collect();
        format!(
            r#"{{"auditing":true,"now_ns":{},"full":{},"sites":[{}],"objects":[{}],"unowned":[{}],"findings":[{}]}}"#,
            self.machine.time / 1000,
            sites.len() >= armv6m::bus::SITES,
            sites_json.join(","),
            objects_json.join(","),
            unowned_json.join(","),
            findings.join(",")
        )
    }

    /// `board/writers/{owner}`: the code that last wrote each word of
    /// `owner`'s RAM (its writable sections and its stack): each writer
    /// with its code and source line, how many words it holds, the latest
    /// stamp among them, and the runs of words, `[address, length]`. What
    /// colours a source file by the memory each of its lines holds.
    pub fn writers_json(&self, owner: &str, map: Option<&Result<Image, String>>, read: ReadFile) -> Option<String> {
        let image = map?.as_ref().ok()?;
        let place = if owner == "kernel" { &image.kernel } else { image.owner(owner)? };
        let mut ranges: Vec<(u32, u32)> = place.sections.iter().filter(|s| s.writable).map(|s| (s.base, s.size)).collect();
        ranges.extend(place.stack);
        // writer → (words, latest, runs)
        let mut by: BTreeMap<u32, (u32, u64, Vec<(u32, u32)>)> = BTreeMap::new();
        for (base, size) in ranges {
            for a in (base & !3..base.saturating_add(size)).step_by(4) {
                let Some(mark) = self.machine.bus.last_write(a) else { continue };
                let entry = by.entry(mark.writer).or_default();
                entry.0 += 1;
                entry.1 = entry.1.max(mark.stamp);
                match entry.2.last_mut() {
                    Some((start, length)) if *start + *length == a => *length += 4,
                    _ => entry.2.push((a, 4)),
                }
            }
        }
        let writers: Vec<String> = by
            .iter()
            .map(|(&writer, (words, latest, runs))| {
                format!(
                    r#"{{"writer":{},"words":{words},"latest":{latest},"runs":[{}]}}"#,
                    self.writer_json(writer, map, read),
                    runs.iter().map(|(a, n)| format!("[{a},{n}]")).collect::<Vec<_>>().join(",")
                )
            })
            .collect();
        Some(format!(
            r#"{{"owner":{},"now":{},"writers":[{}]}}"#,
            quoted(owner),
            armv6m::bus::write_stamp(self.machine.time),
            writers.join(",")
        ))
    }

    fn object_json(&self, o: &Object) -> String {
        format!(
            r#"{{"owner":{},"name":{},"address":{},"size":{},"type":{}}}"#,
            quoted(&o.owner),
            quoted(&o.name),
            o.address,
            o.size,
            o.ty.map_or("null".into(), |t| t.to_string())
        )
    }
}
