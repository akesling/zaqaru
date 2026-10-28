//! The container as a store: the isotope Server Protocol, served by the
//! kernel.
//!
//! A Block has two faces. Inside it is a StructFS client, reading and
//! writing paths under `/iso`; outside it is a StructFS store that others
//! read and write. The runtime turns an outside read into a Request the
//! Block reads from `/iso/server/requests/pending`, and the Block answers by
//! writing a Response to the Request's `respond_to` path. This module is
//! the container's outside face: the paths it serves describe the machine
//! — every process and thread, a thread's registers, a process's memory
//! map and descriptors, the block cache, what the run has cost — and are
//! what a debugger reads.
//!
//! **Serving a Request never changes anything the guest can observe.** It
//! reads kernel state and writes a Response, and that is the whole of it.
//! The three paths that take a write keep to that: they refill, flush or
//! zero kernel caches whose contents the guest cannot tell from their
//! absence, so that a snapshot can leave them out (see `write`).
//! The rule is what lets the host keep `/iso/server` outside the tape: the
//! answers to `requests/pending` are not inputs to the run, so a recording
//! does not keep them and a replay does not check them, and a debugger can
//! ask questions during a replay without the run diverging.
//!
//! Values are JSON, written and parsed by the few dozen lines below rather
//! than a library the module would otherwise never need.

use core::fmt::Write as _;

use crate::abi::{Store, StoreOutcome};
use crate::machine::Machine as _;
use crate::paths;

use super::System;

/// What the container's store serves, as the manifest and
/// `/iso/self/interface` declare it. The disassembly path is declared only
/// when it is compiled in.
macro_rules! interface {
    ($disassembly:literal) => {
        concat!(
            r#"{"name":"zaqaru-container","version":"0.1.0","serialization":"application/json","paths":{"statistics":{"read":"instructions retired and accelerated, blocks decoded, the current pid"},"processes":{"read":"every process — pid, parent, the path it was started from and its comm — and its threads, with what each is parked on"},"processes/{pid}/threads/{tid}/registers":{"read":"the general registers, rip, the segment base and the flags of one thread"},"processes/{pid}/maps":{"read":"the process's memory map, as /proc/self/maps renders it"},"processes/{pid}/descriptors":{"read":"the process's open descriptors, each with the path it was opened by"},"processes/{pid}/files/{path}":{"read":"the process's view of the filesystem at a path: a directory's entries, a file's contents, a symlink's target"},"net":{"read":"every socket in the container: listeners, connections and what is queued each way, and which processes hold each"},"board":{"read":"the board on the container's bench, if the image asked for one: running or halted, simulated time, the LED, faults, exceptions taken, and the core's registers"},"board/trace/{since}/{channels}":{"read":"what has crossed the board's pins and buses — pin levels, I2C and SPI transactions, USART bytes, Ethernet frames — from a record on, on the channels asked for (PB0,I2C2,SPI3,USART1,ETH)"},"board/memory/{address}/{length}":{"read":"up to 4096 bytes of the board's memory — RAM, flash, peripheral registers — as a debugger reads them, hex"},"board/why/{address}":{"read":"where the value at a board address came from, while it is being followed: each copy back to the read of a peripheral's data register, and the trace record of the byte"},"board/tagged":{"read":"while values are being followed, the runs of the board's memory that hold values that came in from outside, with where each is and the read it came from"},"board/provenance":{"write":"`on`: follow where the board's values go from this instant; `off`: stop"},"board/tasks":{"read":"the board's memory as its Hubris archive lays it out: the kernel and each task, with its sections, its stack and how many symbols it has"},"board/whatis/{address}":{"read":"what an address of the board's is: the memory or peripheral it is in, whose it is and which part — code, data, stack — and the function or static it falls in"},"board/writes/{since}/{cell}":{"read":"which cells of so many bytes of the board's RAMs were written at or since a stamp — the board's time in microseconds, plus one — and when each last was; the cells made larger, if need be, to be at most 65536"},"board/writes/{since}/{cell}/{address}/{length}":{"read":"the same, for part of one RAM, its cells numbered from the address"},"board/written/{address}/{length}":{"read":"when each word of the board's RAM from an address was last written, and the code, the probe or the DMA that wrote it"},"board/symbols/{owner}":{"read":"the statics in the RAM of the kernel or of a task, by name, from the board's archive"},"board/source/{pc}":{"read":"whose code an address of the board's is, and the function, file and line it came from, with every call inlined into it, from the archive's DWARF"},"board/typed/{owner}":{"read":"the statics in the RAM of the kernel or of a task, each with its type, and every type they are made of, from the archive's DWARF"},"board/types/{owner}/{ids}":{"read":"types of the kernel's or a task's DWARF by number, comma-separated, and every type they are made of"},"board/stack/{owner}":{"read":"the stack of the kernel or of a task stepped back from where it is stopped, frame by frame: each frame's code and source line, and its variables with their bytes and types"},"board/file/{path}":{"read":"a source file the archive's DWARF names, as the build kept it beside the archive"},"board/digest/{which}":{"read":"`memory`: a digest of each of the board's RAMs and its flash, and where the board stands, to tell two runs of it apart"},"usb":{"read":"the USB devices the container's programs can open — the board's probe and any the host granted — with what each has carried and its latest transfers"},"processes/{pid}/memory/{address}/{length}":{"read":"up to 4096 bytes of the running process's memory, hex"},"processes/{pid}/mapped":{"read":"the pages the process can reach, as [start, end) runs"},"cache":{"read":"the running process's block cache"},"layout":{"read":"where the guest block and the program region below the module's data are in linear memory, and the ranges the kernel never writes again"},"caches":{"read":"what the kernel keeps that a snapshot need not: decompressed files, decoded blocks, pooled page buffers"},"caches/blocks":{"read":"every process's block cache: live blocks and bytes","write":"`flush`: throw every block cache away, zeroing what is freed"},"caches/decompressed":{"read":"every decompressed file's buffer: address and length","write":"`refill`: decompress every cached file again into its buffer"},"caches/pool":{"read":"pooled page buffers","write":"`zero`: zero the pooled buffers"},"meta/":{"read":"which paths are readable"}"#,
            $disassembly,
            "}}"
        )
    };
}
#[cfg(feature = "disassembly")]
pub const INTERFACE: &str = interface!(
    r#","processes/{pid}/threads/{tid}/disassembly":{"read":"the instructions from the thread's rip, as text"}"#
);
#[cfg(not(feature = "disassembly"))]
pub const INTERFACE: &str = interface!("");

/// The most memory one read hands back.
const MEMORY_READ_CAP: u64 = 4096;

/// One Request, as the runtime queued it. `data` is kept when it is a
/// string, which is what the two writable paths take.
struct Request {
    op: String,
    path: String,
    data: Option<String>,
    respond_to: String,
}

impl<'a, S: Store + Clone> System<'a, S> {
    /// Answers every Request the host has queued.
    ///
    /// Called where the kernel already talks to the host between turns —
    /// once a slice, and whenever a run returns to the host — so a debugger
    /// holding the machine at an instant reads that instant.
    pub fn serve(&mut self) {
        let mut pending = Vec::new();
        let asked = self
            .current()
            .kernel
            .store
            .read(paths::SERVER_PENDING, &mut pending);
        if asked != StoreOutcome::Present || pending.is_empty() {
            return;
        }
        let requests = match parse_requests(&pending) {
            Some(requests) => requests,
            None => {
                let message = "kernel: the host's request batch is not the JSON the Server Protocol specifies";
                crate::report_to(&mut self.current().kernel, message);
                return;
            }
        };
        for request in requests {
            let response = match request.op.as_str() {
                "read" => match self.read(&request.path) {
                    Ok(value) => format!(r#"{{"result":"ok","value":{value}}}"#),
                    Err(Refusal::NotFound) => error("not_found", &format!("the container serves no {}", request.path)),
                    Err(Refusal::Unavailable(why)) => error("unavailable", &why),
                },
                "write" => match self.write(&request.path, request.data.as_deref().unwrap_or("")) {
                    Ok(value) => format!(r#"{{"result":"ok","value":{value}}}"#),
                    Err(Refusal::NotFound) => error("not_writable", &format!("the container does not take writes at {}", request.path)),
                    Err(Refusal::Unavailable(why)) => error("unavailable", &why),
                },
                other => error("invalid_path", &format!("unknown operation {other}")),
            };
            let path = segments(&request.respond_to);
            let path: Vec<&[u8]> = path.iter().map(Vec::as_slice).collect();
            let _ = self.current().kernel.store.write(&path, response.as_bytes());
        }
    }

    /// The value at a path of the container's store, as JSON.
    fn read(&mut self, path: &str) -> Result<String, Refusal> {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["statistics"] => Ok(self.statistics()),
            ["processes"] => Ok(self.processes()),
            ["processes", pid, "maps"] => {
                let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
                let maps = self.containers[index].process.kernel.render_maps();
                Ok(quoted(&maps))
            }
            ["processes", pid, "descriptors"] => {
                let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
                Ok(self.descriptors(index))
            }
            ["processes", pid, "files", rest @ ..] => {
                let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
                self.files(index, rest)
            }
            ["net"] => Ok(self.net()),
            // The board is container state, so any process's kernel holds
            // it; without one there is nothing here to read.
            ["board"] => match &*self.containers[self.current].process.kernel.board.borrow() {
                crate::board::Slot::Present(board) => Ok(board.json()),
                _ => Err(Refusal::NotFound),
            },
            // What has crossed the board's pins and buses, from a record on
            // (or the last so many, negative), on the channels asked for.
            ["board", "trace", since, channels @ ..] => {
                let since = since.parse().ok().ok_or(Refusal::NotFound)?;
                let channels: Vec<&str> =
                    channels.first().map(|c| c.split(',').filter(|c| !c.is_empty()).collect()).unwrap_or_default();
                match &*self.containers[self.current].process.kernel.board.borrow() {
                    crate::board::Slot::Present(board) => Ok(board.trace_json(since, &channels)),
                    _ => Err(Refusal::NotFound),
                }
            }
            // The board's memory as a debugger reads it, and a digest of all
            // of it (`memory`: each RAM and the flash) to tell runs apart.
            ["board", "memory", address, length] => {
                let address = number(address).ok_or(Refusal::NotFound)?;
                let length = number(length).ok_or(Refusal::NotFound)?;
                match &*self.containers[self.current].process.kernel.board.borrow() {
                    crate::board::Slot::Present(board) => Ok(board.memory_json(address as u32, length as u32)),
                    _ => Err(Refusal::NotFound),
                }
            }
            // What the board's memory is: its tasks' layout, and what one
            // address is, from the board's Hubris archive and the chip's
            // memory map.
            // When each cell of the board's RAMs was last written, those
            // written since a stamp.
            ["board", "writes", since, cell] => {
                let since = number(since).ok_or(Refusal::NotFound)?;
                let cell = number32(cell)?;
                match &*self.containers[self.current].process.kernel.board.borrow() {
                    crate::board::Slot::Present(board) => Ok(board.writes_json(since, cell)),
                    _ => Err(Refusal::NotFound),
                }
            }
            ["board", "writes", since, cell, base, length] => {
                let since = number(since).ok_or(Refusal::NotFound)?;
                let (cell, base, length) = (number32(cell)?, number32(base)?, number32(length)?);
                match &*self.containers[self.current].process.kernel.board.borrow() {
                    crate::board::Slot::Present(board) => Ok(board.writes_in_json(since, cell, base, length)),
                    _ => Err(Refusal::NotFound),
                }
            }
            ["board", "file", rest @ ..] if !rest.is_empty() => {
                let kernel = &self.containers[self.current].process.kernel;
                let slot = kernel.board.borrow();
                let crate::board::Slot::Present(board) = &*slot else { return Err(Refusal::NotFound) };
                let path = rest.join("/");
                let text = board.source_file(&path, &|path: &[u8]| kernel.read_file(path)).ok_or(Refusal::NotFound)?;
                Ok(format!(r#"{{"path":{},"text":{}}}"#, quoted(&format!("/{path}")), quoted(&String::from_utf8_lossy(&text))))
            }
            ["board", "tasks"]
            | ["board", "whatis", _]
            | ["board", "why", _]
            | ["board", "tagged"]
            | ["board", "symbols", _]
            | ["board", "written", _, _]
            | ["board", "source", _]
            | ["board", "typed", _]
            | ["board", "types", _, _]
            | ["board", "stack", _] => {
                let kernel = &self.containers[self.current].process.kernel;
                let slot = kernel.board.borrow();
                let crate::board::Slot::Present(board) = &*slot else { return Err(Refusal::NotFound) };
                let read = |path: &[u8]| kernel.read_file(path);
                let map = board.memory_map(read);
                match parts.as_slice() {
                    [_, "whatis", address] => {
                        let address = number(address).ok_or(Refusal::NotFound)?;
                        Ok(board.whatis_json(address as u32, map.as_deref()))
                    }
                    [_, "why", address] => {
                        let address = number(address).ok_or(Refusal::NotFound)?;
                        Ok(board.why_json(address as u32, map.as_deref()))
                    }
                    [_, "tagged"] => Ok(board.tagged_json(200, map.as_deref())),
                    [_, "symbols", owner] => board.symbols_json(owner, map.as_deref()).ok_or(Refusal::NotFound),
                    [_, "written", address, length] => Ok(board.written_json(number32(address)?, number32(length)?, map.as_deref(), &read)),
                    [_, "source", pc] => Ok(board.source_json(number32(pc)?, map.as_deref(), &read)),
                    [_, "typed", owner] => board.typed_json(owner, map.as_deref(), &read).ok_or(Refusal::NotFound),
                    [_, "types", owner, ids] => board.types_json(owner, ids, &read).ok_or(Refusal::NotFound),
                    [_, "stack", owner] => board.stack_json(owner, map.as_deref(), &read).ok_or(Refusal::NotFound),
                    _ => Ok(board.tasks_json(map.as_deref())),
                }
            }
            ["board", "digest", "memory"] => match &*self.containers[self.current].process.kernel.board.borrow() {
                crate::board::Slot::Present(board) => Ok(board.digest_json()),
                _ => Err(Refusal::NotFound),
            },
            // The USB devices, which are container state for the same reason.
            ["usb"] => Ok(self.containers[self.current].process.kernel.usb.borrow().json()),
            ["processes", pid, "threads", tid, "registers"] => {
                let (index, position) = self.thread_index(pid, tid)?;
                Ok(registers(&self.containers[index].process.kernel.machine.threads.all()[position].tcb))
            }
            ["processes", pid, "threads", tid, "disassembly"] => {
                let (index, position) = self.thread_index(pid, tid)?;
                self.in_place(index)?;
                let rip = self.containers[index].process.kernel.machine.threads.all()[position].tcb.rip;
                Ok(disassembly(&self.containers[index].process.kernel.pages, rip))
            }
            ["processes", pid, "mapped"] => {
                let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
                let mut out = String::from(r#"{"ranges":["#);
                let mut run: Option<(u64, u64)> = None;
                let mut first = true;
                let close = |out: &mut String, run: (u64, u64), first: &mut bool| {
                    if !*first {
                        out.push(',');
                    }
                    *first = false;
                    let _ = write!(out, r#"["{:#x}","{:#x}"]"#, run.0, run.1);
                };
                for page in self.containers[index].process.kernel.pages.mapped_pages() {
                    match run {
                        Some((start, end)) if end == page => run = Some((start, page + 4096)),
                        Some(done) => {
                            close(&mut out, done, &mut first);
                            run = Some((page, page + 4096));
                        }
                        None => run = Some((page, page + 4096)),
                    }
                }
                if let Some(done) = run {
                    close(&mut out, done, &mut first);
                }
                out.push_str("]}");
                Ok(out)
            }
            ["processes", pid, "memory", address, length] => {
                let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
                self.in_place(index)?;
                let address = number(address).ok_or(Refusal::NotFound)?;
                let length = number(length).ok_or(Refusal::NotFound)?.min(MEMORY_READ_CAP);
                Ok(memory(&self.containers[index].process.kernel.pages, address, length))
            }
            ["layout"] => {
                let start = self.current().kernel.machine.guest_base();
                let end = start.saturating_add(crate::syscall::GUEST_ADDRESS_SPACE);
                let mut out = format!(r#"{{"guest_block":{{"start":"{start:#x}","end":"{end:#x}"}},"#);
                if let Some((low, high)) = crate::image::program_region() {
                    let _ = write!(out, r#""program_region":{{"start":"{low:#x}","end":"{high:#x}"}},"#);
                }
                out.push_str(r#""frozen":["#);
                for (index, (address, length)) in crate::image::frozen().into_iter().enumerate() {
                    let comma = if index == 0 { "" } else { "," };
                    let _ = write!(out, r#"{comma}{{"start":"{address:#x}","end":"{:#x}"}}"#, address + length);
                }
                out.push_str("]}");
                Ok(out)
            }
            ["caches"] => {
                let files = crate::image::decompressed_cache();
                let (live, bytes) = self.blocks_held();
                Ok(format!(
                    r#"{{"decompressed":{{"files":{},"bytes":{}}},"blocks":{{"live":{live},"bytes":{bytes}}},"pool":{{"pages":{}}}}}"#,
                    files.files,
                    files.bytes,
                    crate::resident::pool_pages()
                ))
            }
            ["caches", "blocks"] => {
                let (live, bytes) = self.blocks_held();
                Ok(format!(r#"{{"live":{live},"bytes":{bytes}}}"#))
            }
            ["caches", "decompressed"] => {
                let files = crate::image::decompressed_cache();
                let mut out = format!(r#"{{"files":{},"bytes":{},"ranges":["#, files.files, files.bytes);
                for (position, (address, length)) in files.ranges.iter().enumerate() {
                    if position > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, r#"{{"address":"{address:#x}","length":{length}}}"#);
                }
                out.push_str("]}");
                Ok(out)
            }
            ["caches", "pool"] => Ok(format!(r#"{{"pages":{}}}"#, crate::resident::pool_pages())),
            ["cache"] => {
                let cache = &self.current().cache;
                Ok(format!(
                    r#"{{"decoded":{},"flushes":{},"live":{},"accelerates":{}}}"#,
                    cache.decoded,
                    cache.flushes,
                    cache.len(),
                    cache.accelerates()
                ))
            }
            ["meta"] | ["meta", ..] => Ok(String::from(
                r#"{"paths":{"statistics":{"readable":true,"writable":false},"processes":{"readable":true,"writable":false},"processes/{pid}/threads/{tid}/registers":{"readable":true,"writable":false},"processes/{pid}/threads/{tid}/disassembly":{"readable":true,"writable":false},"processes/{pid}/maps":{"readable":true,"writable":false},"processes/{pid}/descriptors":{"readable":true,"writable":false},"processes/{pid}/files/{path}":{"readable":true,"writable":false},"net":{"readable":true,"writable":false},"board":{"readable":true,"writable":false},"board/trace/{since}/{channels}":{"readable":true,"writable":false},"board/memory/{address}/{length}":{"readable":true,"writable":false},"board/why/{address}":{"readable":true,"writable":false},"board/provenance":{"readable":false,"writable":true},"board/tagged":{"readable":true,"writable":false},"board/tasks":{"readable":true,"writable":false},"board/whatis/{address}":{"readable":true,"writable":false},"board/writes/{since}/{cell}":{"readable":true,"writable":false},"board/writes/{since}/{cell}/{address}/{length}":{"readable":true,"writable":false},"board/written/{address}/{length}":{"readable":true,"writable":false},"board/symbols/{owner}":{"readable":true,"writable":false},"board/source/{pc}":{"readable":true,"writable":false},"board/typed/{owner}":{"readable":true,"writable":false},"board/types/{owner}/{ids}":{"readable":true,"writable":false},"board/stack/{owner}":{"readable":true,"writable":false},"board/file/{path}":{"readable":true,"writable":false},"board/digest/{which}":{"readable":true,"writable":false},"usb":{"readable":true,"writable":false},"processes/{pid}/memory/{address}/{length}":{"readable":true,"writable":false},"processes/{pid}/mapped":{"readable":true,"writable":false},"cache":{"readable":true,"writable":false},"layout":{"readable":true,"writable":false},"caches":{"readable":true,"writable":false},"caches/decompressed":{"readable":true,"writable":true},"caches/blocks":{"readable":true,"writable":true},"caches/pool":{"readable":true,"writable":true}}}"#,
            )),
            _ => Err(Refusal::NotFound),
        }
    }

    /// The two writes the store takes, both kernel-only and invisible to the
    /// guest: they exist so that a snapshot can leave out what they can
    /// put back.
    fn write(&mut self, path: &str, data: &str) -> Result<String, Refusal> {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (parts.as_slice(), data.trim()) {
            (["caches", "decompressed"], "refill") => match crate::image::refill_decompressed() {
                Ok((files, bytes)) => Ok(format!(r#"{{"files":{files},"bytes":{bytes}}}"#)),
                Err(why) => Err(Refusal::Unavailable(format!("refilling the decompressed files failed: {why:?}"))),
            },
            // Following where the board's values go, from this instant: a
            // seek turns it on in the container it re-executes, never the
            // live one.
            (["board", "provenance"], on @ ("on" | "off")) => {
                let kernel = &self.containers[self.current].process.kernel;
                let mut slot = kernel.board.borrow_mut();
                let crate::board::Slot::Present(board) = &mut *slot else { return Err(Refusal::NotFound) };
                Ok(board.follow_values(on == "on"))
            }
            (["caches", "pool"], "zero") => Ok(format!(r#"{{"pages":{}}}"#, crate::resident::zero_pool())),
            (["caches", "blocks"], "flush") => {
                let (live, bytes) = self.blocks_held();
                // Zero what the flush frees, so that a snapshot taken next
                // has nothing of it to keep; then back to plain frees.
                crate::SCRUB_FREED.store(true, core::sync::atomic::Ordering::Relaxed);
                for container in self.containers.iter_mut() {
                    let process = &mut container.process;
                    process.cache.flush(&mut process.kernel.pages);
                }
                crate::SCRUB_FREED.store(false, core::sync::atomic::Ordering::Relaxed);
                Ok(format!(r#"{{"flushed":{live},"bytes":{bytes}}}"#))
            }
            (["caches", "decompressed"] | ["caches", "pool"] | ["caches", "blocks"], other) => {
                Err(Refusal::Unavailable(format!("{path} does not take {other:?}")))
            }
            _ => Err(Refusal::NotFound),
        }
    }

    /// Every process's block cache: live blocks, and about how many bytes.
    fn blocks_held(&self) -> (usize, usize) {
        self.containers
            .iter()
            .fold((0, 0), |(live, bytes), container| (live + container.process.cache.len(), bytes + container.process.cache.footprint()))
    }

    /// Which container and which of its threads a path names.
    fn thread_index(&self, pid: &str, tid: &str) -> Result<(usize, usize), Refusal> {
        let index = self.container_index(pid.parse().ok().ok_or(Refusal::NotFound)?).ok_or(Refusal::NotFound)?;
        let tid: i32 = tid.parse().ok().ok_or(Refusal::NotFound)?;
        let position = self.containers[index]
            .process
            .kernel
            .machine
            .threads
            .all()
            .iter()
            .position(|thread| thread.tid == tid)
            .ok_or(Refusal::NotFound)?;
        Ok((index, position))
    }

    /// Whether a process's memory is the memory in place. Every process
    /// maps the same range, and only the running one's bytes are at those
    /// addresses — see `resident` — so the store reads memory only for the
    /// process whose turn it is, and says so otherwise.
    fn in_place(&self, index: usize) -> Result<(), Refusal> {
        if self.containers[index].pid == self.current_pid() {
            Ok(())
        } else {
            Err(Refusal::Unavailable(format!(
                "pid {} is not running; only the running process's memory ({}) is in place",
                self.containers[index].pid,
                self.current_pid()
            )))
        }
    }

    fn container_index(&self, pid: i32) -> Option<usize> {
        self.containers.iter().position(|container| container.pid == pid)
    }

    /// What the run has cost so far, and whose turn it is.
    fn statistics(&self) -> String {
        format!(
            r#"{{"retired":{},"accelerated":{},"decoded":{},"current":{}}}"#,
            self.retired(),
            self.accelerated(),
            self.decoded(),
            self.current_pid()
        )
    }

    /// Every process and every thread: the stall report, structured.
    fn processes(&self) -> String {
        let mut out = String::from(r#"{"current":"#);
        let _ = write!(out, "{},\"processes\":[", self.current_pid());
        for (index, container) in self.containers.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                r#"{{"pid":{},"parent":{},"exe":{},"comm":{},"displaced":{},"state":{},"threads":["#,
                container.pid,
                container.parent,
                quoted(&container.process.kernel.executable),
                quoted(&container.process.kernel.comm),
                crate::resident::displaced_pages(container.process.kernel.machine.token),
                match container.status {
                    Some(super::Ending::Exited(code)) => format!(r#"{{"exited":{code}}}"#),
                    Some(super::Ending::Signalled(signal)) => format!(r#"{{"signalled":{signal}}}"#),
                    None => String::from(r#""live""#),
                }
            );
            for (position, thread) in container.process.kernel.machine.threads.all().iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                let _ = write!(
                    out,
                    r#"{{"tid":{},"rip":"{:#x}","retired":{},"state":{}}}"#,
                    thread.tid,
                    thread.tcb.rip,
                    thread.tcb.retired,
                    quoted(&self.describe_state(container, thread))
                );
            }
            out.push_str("]}");
        }
        out.push_str("]}");
        out
    }

    fn descriptors(&self, index: usize) -> String {
        let files = &self.containers[index].process.kernel.files;
        let mut out = String::from("[");
        for (position, (fd, what)) in files.open_descriptors().enumerate() {
            if position > 0 {
                out.push(',');
            }
            let (offset, flags, cloexec) = match files.description(fd) {
                Ok(open) => (open.offset, open.flags, files.close_on_exec(fd).unwrap_or(false)),
                Err(_) => (0, 0, false),
            };
            let path = match files.name(fd) {
                Some(name) => quoted(&String::from_utf8_lossy(name)),
                None => String::from("null"),
            };
            let _ = write!(
                out,
                r#"{{"fd":{fd},"what":{},"path":{path},"offset":{offset},"flags":{flags},"cloexec":{cloexec}}}"#,
                quoted(&what)
            );
        }
        out.push(']');
        out
    }

    /// A process's view of the filesystem at a path — its own view, since a
    /// fork copies the writable layer and the working directory. The final
    /// component is not followed, so a symlink shows as one; a directory
    /// lists its entries, a regular file gives its contents up to a cap.
    fn files(&self, index: usize, rest: &[&str]) -> Result<String, Refusal> {
        use crate::image::file_type;
        use crate::vfs::Lookup;
        let mut path = String::from("/");
        path.push_str(&rest.join("/"));
        let vfs = &self.containers[index].process.kernel.vfs;
        let cwd = {
            let mut into = vec![0u8; crate::file::PATH_MAX];
            match vfs.absolute_path(vfs.working_directory(), &mut into) {
                Ok(length) => quoted(&String::from_utf8_lossy(&into[..length])),
                Err(_) => String::from("null"),
            }
        };
        let refused = |errno: crate::errno::Errno| match errno {
            crate::errno::Errno::NoEntry => Refusal::NotFound,
            other => Refusal::Unavailable(format!("{path}: {}", other.name())),
        };
        let vnode = vfs.resolve(vfs.root(), path.as_bytes(), Lookup::NO_FOLLOW).map_err(refused)?;
        let inode = vfs.inode(vnode).map_err(refused)?;
        let filesystem = vfs.filesystem_of(vnode).map_err(refused)?;
        let mut out = format!(r#"{{"path":{},"cwd":{cwd},"mode":"{:o}","size":{},"mtime":{},"#, quoted(&path), inode.mode, inode.size, inode.mtime_sec);
        match inode.file_type() {
            file_type::DIRECTORY => {
                out.push_str(r#""kind":"directory","entries":["#);
                let mut entries = filesystem.entries(&inode, vnode.inode).map_err(refused)?;
                let mut count = 0;
                while let Some(entry) = entries.next().map_err(refused)? {
                    if count == FILES_ENTRY_CAP {
                        break;
                    }
                    if count > 0 {
                        out.push(',');
                    }
                    count += 1;
                    let (kind, size) = match filesystem.inode(entry.inode) {
                        Ok(child) => (kind_name(child.file_type()), child.size),
                        Err(_) => ("unknown", 0),
                    };
                    let _ = write!(out, r#"{{"name":{},"kind":"{kind}","size":{size}}}"#, quoted(&String::from_utf8_lossy(entry.name)));
                }
                out.push(']');
                let truncated = entries.next().map_err(refused)?.is_some();
                let _ = write!(out, r#","truncated":{truncated}}}"#);
            }
            file_type::REGULAR => {
                let contents = filesystem.contents(&inode, vnode.inode).map_err(refused)?;
                let shown = &contents[..contents.len().min(FILES_CONTENT_CAP)];
                out.push_str(r#""kind":"file","#);
                match core::str::from_utf8(shown) {
                    Ok(text) if text.chars().all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t')) => {
                        let _ = write!(out, r#""text":{}"#, quoted(text));
                    }
                    _ => {
                        let _ = write!(out, r#""hex":"{}""#, hex(&shown[..shown.len().min(FILES_HEX_CAP)]));
                    }
                }
                let _ = write!(out, r#","truncated":{}}}"#, contents.len() > shown.len());
            }
            file_type::SYMLINK => {
                let target = filesystem.symlink_target(&inode, vnode.inode).map_err(refused)?;
                let _ = write!(out, r#""kind":"symlink","target":{}}}"#, quoted(&String::from_utf8_lossy(target)));
            }
            other => {
                let _ = write!(out, r#""kind":"{}"}}"#, kind_name(other));
            }
        }
        Ok(out)
    }

    /// Every socket in the container, with what it is doing and which
    /// processes hold a descriptor on it.
    fn net(&self) -> String {
        use crate::socket::{Address, State};
        let kernel = &self.containers[self.current].process.kernel;
        let sockets = kernel.sockets.borrow();
        let rings = kernel.rings.borrow();
        let address = |address: &Address| match address {
            Address::Unbound => String::from("null"),
            Address::Inet { address, port } => {
                quoted(&format!("{}.{}.{}.{}:{port}", address >> 24, (address >> 16) & 0xff, (address >> 8) & 0xff, address & 0xff))
            }
            Address::Unix { path, .. } => quoted(&String::from_utf8_lossy(path)),
        };
        let mut out = String::from(r#"{"sockets":["#);
        for (position, (id, socket)) in sockets.all().enumerate() {
            if position > 0 {
                out.push(',');
            }
            let holders: Vec<String> = self
                .containers
                .iter()
                .filter(|container| container.process.kernel.files.socket_ids().any(|held| held == id))
                .map(|container| container.pid.to_string())
                .collect();
            let family = match socket.family {
                crate::socket::family::INET => String::from(r#""inet""#),
                crate::socket::family::UNIX => String::from(r#""unix""#),
                other => other.to_string(),
            };
            let edge = socket.edge.map_or(String::from("null"), |edge| edge.to_string());
            let state = match &socket.state {
                State::Idle => String::from(r#""idle""#),
                State::Bound(at) => format!(r#"{{"bound":{}}}"#, address(at)),
                State::Listening { address: at, backlog, queue } => {
                    format!(r#"{{"listening":{{"address":{},"backlog":{backlog},"queued":{}}}}}"#, address(at), queue.len())
                }
                State::Connected(endpoint) => format!(
                    r#"{{"connected":{{"local":{},"peer":{},"receive_queued":{},"transmit_queued":{},"read_shut":{},"write_shut":{}}}}}"#,
                    address(&endpoint.local),
                    address(&endpoint.peer),
                    rings.queued(endpoint.receive),
                    rings.queued(endpoint.transmit),
                    endpoint.read_shut,
                    endpoint.write_shut
                ),
            };
            let _ = write!(
                out,
                r#"{{"id":{id},"family":{family},"kind":{},"edge":{edge},"references":{},"holders":[{}],"state":{state}}}"#,
                socket.kind,
                socket.references,
                holders.join(",")
            );
        }
        out.push_str("]}");
        out
    }
}

/// How much of a file the `files` path hands back, and how many entries of
/// a directory: a page's worth, not a dump.
const FILES_CONTENT_CAP: usize = 16384;
const FILES_HEX_CAP: usize = 4096;
const FILES_ENTRY_CAP: usize = 4096;

/// A file type's name, as the `files` path says it.
fn kind_name(file_type: u32) -> &'static str {
    use crate::image::file_type;
    match file_type {
        file_type::DIRECTORY => "directory",
        file_type::REGULAR => "file",
        file_type::SYMLINK => "symlink",
        file_type::FIFO => "fifo",
        file_type::CHARACTER => "character",
        file_type::BLOCK => "block",
        file_type::SOCKET => "socket",
        _ => "unknown",
    }
}

/// A thread's registers. Sixty-four-bit values are hex strings, because
/// a JSON number is a double and would lose the high bits.
fn registers(tcb: &cpu::state::Tcb) -> String {
    const NAMES: [&str; 16] = [
        "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12",
        "r13", "r14", "r15",
    ];
    let mut out = String::from("{");
    for (name, value) in NAMES.iter().zip(tcb.registers.iter()) {
        let _ = write!(out, r#""{name}":"{value:#x}","#);
    }
    let _ = write!(
        out,
        r#""rip":"{:#x}","fs_base":"{:#x}","flags":"{:#x}","flags_stale":{},"retired":{}}}"#,
        tcb.rip,
        tcb.fs_base,
        tcb.flags.materialized(),
        match tcb.flags_staleness {
            cpu::state::Staleness::Unknown => "null",
            cpu::state::Staleness::Fresh => "false",
            cpu::state::Staleness::Stale => "true",
        },
        tcb.retired
    );
    out
}

/// Why a read was not answered with a value.
enum Refusal {
    /// The store serves nothing at the path.
    NotFound,
    /// The path exists, but not now, and this is why.
    Unavailable(String),
}

/// A number in a path: hex with `0x`, or decimal.
/// A number in a path that is a board's address, length or size: one that
/// does not fit in 32 bits names nothing.
fn number32(text: &str) -> Result<u32, Refusal> {
    number(text).and_then(|n| u32::try_from(n).ok()).ok_or(Refusal::NotFound)
}

fn number(text: &str) -> Option<u64> {
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// The instructions from `rip`, as the disassembly feature renders them:
/// an array of `{address, bytes, text}`; empty when the feature is not
/// compiled in, which the manifest also says.
fn disassembly(pages: &cpu::space::Space, rip: u64) -> String {
    #[cfg(feature = "disassembly")]
    {
        let mut out = String::from("[");
        for (position, line) in cpu::disassembly::disassemble(pages, rip, 40).iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            let _ = write!(out, r#"{{"address":"{:#x}","bytes":"{}","text":{}}}"#, line.address, hex(&line.bytes), quoted(&line.text));
        }
        out.push(']');
        out
    }
    #[cfg(not(feature = "disassembly"))]
    {
        let _ = (pages, rip);
        String::from("[]")
    }
}

/// `length` bytes at `address` in the running process's memory, as far as
/// they are readable: `{address, bytes}` with the bytes in hex, stopping at
/// the first page the process may not read.
fn memory(pages: &cpu::space::Space, address: u64, length: u64) -> String {
    const PAGE: u64 = 4096;
    let mut held = Vec::with_capacity(length as usize);
    let mut at = address;
    let end = address.saturating_add(length);
    while at < end {
        let page_end = ((at / PAGE) + 1) * PAGE;
        let take = page_end.min(end) - at;
        let mut chunk = vec![0u8; take as usize];
        if pages.read(at, &mut chunk).is_err() {
            break;
        }
        held.extend_from_slice(&chunk);
        at += take;
    }
    format!(r#"{{"address":"{address:#x}","bytes":"{}"}}"#, hex(&held))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn error(kind: &str, message: &str) -> String {
    format!(
        r#"{{"result":"error","error":{{"type":"{kind}","message":{},"retryable":false}}}}"#,
        quoted(message)
    )
}

/// A path string as store segments: `/iso/server/responses/3` is
/// `["iso", "server", "responses", "3"]`.
fn segments(path: &str) -> Vec<Vec<u8>> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.as_bytes().to_vec())
        .collect()
}

/// A JSON string literal, escaped.
pub fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Parses the batch `requests/pending` answers: a JSON array of objects
/// with string `op`, `path` and `respond_to` fields. Any other field —
/// `data`, which a read carries as `null` — is skipped whatever its shape.
fn parse_requests(bytes: &[u8]) -> Option<Vec<Request>> {
    let text = core::str::from_utf8(bytes).ok()?;
    let mut parser = Parser { text, at: 0 };
    parser.skip_space();
    parser.expect('[')?;
    let mut requests = Vec::new();
    parser.skip_space();
    if parser.peek() == Some(']') {
        return Some(requests);
    }
    loop {
        parser.skip_space();
        parser.expect('{')?;
        let (mut op, mut path, mut data, mut respond_to) = (None, None, None, None);
        loop {
            parser.skip_space();
            if parser.peek() == Some('}') {
                parser.at += 1;
                break;
            }
            let key = parser.string()?;
            parser.skip_space();
            parser.expect(':')?;
            parser.skip_space();
            match key.as_str() {
                "op" => op = Some(parser.string()?),
                "path" => path = Some(parser.string()?),
                "respond_to" => respond_to = Some(parser.string()?),
                "data" if parser.peek() == Some('"') => data = Some(parser.string()?),
                _ => parser.skip_value()?,
            }
            parser.skip_space();
            if parser.peek() == Some(',') {
                parser.at += 1;
            }
        }
        requests.push(Request {
            op: op?,
            path: path?,
            data,
            respond_to: respond_to?,
        });
        parser.skip_space();
        match parser.peek()? {
            ',' => parser.at += 1,
            ']' => return Some(requests),
            _ => return None,
        }
    }
}

struct Parser<'t> {
    text: &'t str,
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn skip_space(&mut self) {
        while let Some(c) = self.peek() {
            if !c.is_whitespace() {
                break;
            }
            self.at += c.len_utf8();
        }
    }

    fn expect(&mut self, wanted: char) -> Option<()> {
        (self.peek()? == wanted).then(|| self.at += wanted.len_utf8())
    }

    fn string(&mut self) -> Option<String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            self.at += c.len_utf8();
            match c {
                '"' => return Some(out),
                '\\' => {
                    let escaped = self.peek()?;
                    self.at += escaped.len_utf8();
                    match escaped {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' => {
                            let hex = self.text.get(self.at..self.at + 4)?;
                            self.at += 4;
                            out.push(char::from_u32(u32::from_str_radix(hex, 16).ok()?)?);
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
    }

    /// Skips one value of any shape.
    fn skip_value(&mut self) -> Option<()> {
        self.skip_space();
        match self.peek()? {
            '"' => self.string().map(|_| ()),
            '{' | '[' => {
                let close = if self.peek()? == '{' { '}' } else { ']' };
                self.at += 1;
                loop {
                    self.skip_space();
                    match self.peek()? {
                        c if c == close => {
                            self.at += 1;
                            return Some(());
                        }
                        ',' | ':' => self.at += 1,
                        _ => self.skip_value()?,
                    }
                }
            }
            _ => {
                while let Some(c) = self.peek() {
                    if c == ',' || c == '}' || c == ']' || c.is_whitespace() {
                        break;
                    }
                    self.at += c.len_utf8();
                }
                Some(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_of_requests_parses_and_other_fields_are_skipped() {
        let requests = parse_requests(
            br#"[{"op":"read","path":"processes","data":null,"respond_to":"/iso/server/responses/1"},
                {"op": "write", "path": "x/y", "data": {"a": [1, 2, {"b": "c"}]}, "respond_to": "/iso/server/responses/2"}]"#,
        )
        .expect("parses");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].op, "read");
        assert_eq!(requests[0].path, "processes");
        assert_eq!(requests[1].respond_to, "/iso/server/responses/2");
        assert!(requests[0].data.is_none() && requests[1].data.is_none());
        let written = parse_requests(br#"[{"op":"write","path":"caches/pool","data":"zero","respond_to":"/iso/server/responses/3"}]"#).expect("parses");
        assert_eq!(written[0].data.as_deref(), Some("zero"));
        assert_eq!(segments(&requests[1].respond_to), vec![b"iso".to_vec(), b"server".to_vec(), b"responses".to_vec(), b"2".to_vec()]);
        assert!(parse_requests(b"[]").expect("empty").is_empty());
        assert!(parse_requests(b"not json").is_none());
    }

    #[test]
    fn strings_are_escaped_for_json() {
        assert_eq!(quoted("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
    }
}
