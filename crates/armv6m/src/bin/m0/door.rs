//! `zaqaru-m0 door`: a terminal session with Humility on the bench.
//!
//! ```sh
//! zaqaru-m0 door --archive build-….zip --probe usb --listen 0.0.0.0:80
//! ```
//!
//! `--probe` is the board's USB probe (`usb`), which Humility finds as
//! probe-rs finds any probe plugged in — named by its USB IDs, so that a
//! probe the page has connected beside it does not make the choice
//! ambiguous — or a GDB server's address, which a Humility built with the
//! GDB probe reaches as `gdb:ADDRESS`.
//!
//! Each connection is a session: a banner, a `$ ` prompt, and a line at a
//! time. `humility …` runs Humility against the probe, its output streamed
//! into the connection as it prints; `board` shows the board's own state —
//! under `serve` from the board thread, and on its own, beside a board
//! zaqaru's kernel runs, from `/proc/board`; `help` lists what to try;
//! `exit` ends the session. There is no shell behind it: these are the
//! things a bench with a probe on it is for, and nothing else runs.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};

use crate::usage;

pub fn main(args: Vec<String>) -> ExitCode {
    let mut listen = "0.0.0.0:80".to_string();
    let mut probe = "127.0.0.1:3333".to_string();
    let mut humility = "humility".to_string();
    let mut archive = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "--listen" => &mut listen,
            "--probe" => &mut probe,
            "--humility" => &mut humility,
            "--archive" => archive.insert(String::new()),
            _ => return usage(),
        };
        match args.next() {
            Some(value) => *slot = value,
            None => return usage(),
        }
    }
    let listener = match TcpListener::bind(&listen) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("zaqaru-m0: can't listen on {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("zaqaru-m0: a humility session on {listen}, against the probe {probe}");
    let status = || std::fs::read_to_string("/proc/board").ok();
    accept_requests(listener, Arc::new(FrontDoor::new(humility, archive, probe, Box::new(status))));
    ExitCode::SUCCESS
}

pub(crate) struct FrontDoor {
    humility: String,
    /// The archive Humility is run against: the one the bench started
    /// with, then whichever was last flashed.
    archive: Mutex<Option<String>>,
    /// The probe, as Humility is to reach it: `usb`, or a GDB server's
    /// address.
    probe: String,
    /// The probe has one debugger at a time, so Humility runs one at a time.
    one_at_a_time: Mutex<()>,
    /// The board's state, for `board`; `None` if there is none to ask.
    status: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

impl FrontDoor {
    pub(crate) fn new(
        humility: String,
        archive: Option<String>,
        probe: String,
        status: Box<dyn Fn() -> Option<String> + Send + Sync>,
    ) -> FrontDoor {
        FrontDoor { humility, archive: Mutex::new(archive), probe, one_at_a_time: Mutex::new(()), status }
    }
}

const BANNER: &str = "\
An STM32G031 Nucleo running Hubris, emulated in zaqaru's kernel, with a
debug probe on it and Humility attached through it.
Try `humility tasks`, or `help` for more.
";

const HELP: &str = "\
    humility tasks          the tasks, their state, and what each waits on
    humility tasks -s pong  a task's stack, unwound through its frames
    humility map            the memory each task can reach, from the archive
    humility readvar TICKS  a kernel variable, read live
    humility ringbuf        the ring buffers
    humility hiffy -l       the Idol interfaces hiffy can call
    humility hiffy -c UserLeds.led_toggle -a index=0
                            an IPC call made on the board: toggles the LED
    humility manifest       what the archive says was built
    humility flash -F       flash the archive's image onto the board, and
                            run it
    archives                the Hubris builds there are to flash
    flash NAME              make the board the part NAME was built for and
                            flash it — g031, g070, or one you uploaded
                            (drop an archive on the terminal to upload it)
    humility -p VID:PID …   the same against a probe connected from the
                            page — its USB panel, 'connect a USB
                            device…' — on real hardware
    board                   the board itself: state, time, LED, faults
    exit                    end the session
";

const PROMPT: &[u8] = b"$ ";

/// What the session is, as a terminal's title: OSC 0, the escape a program
/// sets its window's title with, which a terminal shows and does not print.
const TITLE: &[u8] = b"\x1b]0;humility \xe2\x80\x94 Hubris on an STM32G031 Nucleo\x07";

pub(crate) fn accept_requests(listener: TcpListener, door: Arc<FrontDoor>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let door = door.clone();
        std::thread::spawn(move || {
            let _ = session(stream, &door);
        });
    }
}

/// One terminal: a line at a time until `exit` or the other end closes.
fn session(stream: TcpStream, door: &FrontDoor) -> std::io::Result<()> {
    // No read timeout: zaqaru's kernel refuses SO_RCVTIMEO outright, and a
    // session nobody types into costs one parked thread.
    let mut lines = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    out.write_all(TITLE)?;
    out.write_all(BANNER.as_bytes())?;
    out.write_all(PROMPT)?;
    let mut line = String::new();
    loop {
        line.clear();
        if lines.read_line(&mut line)? == 0 {
            break;
        }
        let words = split(line.trim());
        match words.first().map(String::as_str) {
            None => {}
            Some("exit" | "quit" | "logout") => break,
            Some("help") => out.write_all(HELP.as_bytes())?,
            Some("board") => match (door.status)() {
                Some(status) => out.write_all(status.as_bytes())?,
                None => out.write_all(b"no board answered\n")?,
            },
            Some("humility") => door.humility(&words[1..], &out)?,
            Some("archives") => out.write_all(door.archives().as_bytes())?,
            Some("upload") => match (words.get(1), words.get(2).and_then(|n| n.parse::<usize>().ok())) {
                (Some(name), Some(size)) if size <= UPLOAD_LIMIT => {
                    let mut bytes = vec![0u8; size];
                    lines.read_exact(&mut bytes)?;
                    out.write_all(upload(name, &bytes).as_bytes())?;
                }
                _ => out.write_all(format!("upload NAME SIZE, then SIZE bytes; at most {} MiB\n", UPLOAD_LIMIT >> 20).as_bytes())?,
            },
            Some("flash") => match words.get(1) {
                Some(name) => door.flash(name, &out)?,
                None => out.write_all(b"flash NAME: an archive `archives` lists, or one uploaded\n")?,
            },
            Some(other) => out.write_all(format!("{other}: not here — this bench runs humility, board, help and exit\n").as_bytes())?,
        }
        out.write_all(PROMPT)?;
    }
    out.shutdown(Shutdown::Both)
}

impl FrontDoor {
    /// Runs Humility with `args`, its output going straight into the
    /// session, so it arrives as Humility prints it.
    fn humility(&self, args: &[String], out: &TcpStream) -> std::io::Result<()> {
        self.run_humility(args, out).map(|_| ())
    }

    /// The same, answering whether Humility succeeded.
    fn run_humility(&self, args: &[String], out: &TcpStream) -> std::io::Result<bool> {
        let _turn = self.one_at_a_time.lock().unwrap_or_else(|e| e.into_inner());
        let mut command = Command::new(&self.humility);
        command
            .args(args)
            .env("HUMILITY_PROBE", match self.probe.as_str() {
                "usb" => format!("{:04x}:{:04x}", armv6m::cmsis_dap::VENDOR_ID, armv6m::cmsis_dap::PRODUCT_ID),
                address => format!("gdb:{address}"),
            })
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(out.try_clone()?)))
            .stderr(Stdio::from(OwnedFd::from(out.try_clone()?)));
        if let Some(archive) = &*self.archive.lock().unwrap_or_else(|e| e.into_inner()) {
            command.env("HUMILITY_ARCHIVE", archive);
        }
        let mut out = out;
        match command.status() {
            Ok(status) if status.success() => Ok(true),
            Ok(status) => out.write_all(format!("({status})\n").as_bytes()).map(|()| false),
            Err(e) => out.write_all(format!("can't run {}: {e}\n", self.humility).as_bytes()).map(|()| false),
        }
    }
}

/// Where the image's own archives are, and where uploaded ones go.
const ARCHIVES: &str = "/hubris/archives";
const UPLOADS: &str = "/uploads";
/// The largest archive `upload` takes.
const UPLOAD_LIMIT: usize = 16 << 20;

/// Saves an uploaded archive under [`UPLOADS`], by its base name alone, and
/// says what happened.
fn upload(name: &str, bytes: &[u8]) -> String {
    let name: String = name
        .rsplit('/')
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect();
    if name.is_empty() || name.starts_with('.') {
        return "upload: name the archive\n".into();
    }
    let path = format!("{UPLOADS}/{name}");
    match std::fs::create_dir_all(UPLOADS).and_then(|()| std::fs::write(&path, bytes)) {
        Ok(()) => format!("uploaded {name}, {} bytes — `flash {name}` puts it on the board\n", bytes.len()),
        Err(e) => format!("upload: {path}: {e}\n"),
    }
}

/// The part a Hubris archive was built for, from the `chip: Some("…")` its
/// `img/flash.ron` says.
fn part_of(flash_ron: &str) -> Option<&str> {
    let after = flash_ron.split("Some(\"").nth(1)?;
    Some(&after[..after.find('"')?])
}

impl FrontDoor {
    /// The archives there are to flash: the image's, and any uploaded.
    fn archives(&self) -> String {
        let mut out = String::new();
        for directory in [ARCHIVES, UPLOADS] {
            let Ok(entries) = std::fs::read_dir(directory) else { continue };
            let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            for name in names.iter().filter(|n| n.ends_with(".zip")) {
                out += &format!("    {:<24} {directory}/{name}\n", name.trim_end_matches(".zip"));
            }
        }
        let current = self.archive.lock().unwrap_or_else(|e| e.into_inner()).clone();
        out += &format!("on the board: {}\n", current.as_deref().unwrap_or("nothing named"));
        out
    }

    /// An archive by name — `g070`, `g070.zip`, an uploaded one's name — or
    /// by path.
    fn find(name: &str) -> Option<String> {
        if name.contains('/') {
            return std::path::Path::new(name).is_file().then(|| name.to_string());
        }
        let file = if name.ends_with(".zip") { name.to_string() } else { format!("{name}.zip") };
        [UPLOADS, ARCHIVES].iter().map(|d| format!("{d}/{file}")).find(|p| std::path::Path::new(p).is_file())
    }

    /// `flash NAME`: the board becomes the part the archive was built for,
    /// erased, and Humility flashes the archive onto it — `humility flash`,
    /// as on a desk — and is pointed at it from then on.
    fn flash(&self, name: &str, out: &TcpStream) -> std::io::Result<()> {
        let mut out = out;
        let Some(archive) = Self::find(name) else {
            return out.write_all(format!("flash: no archive `{name}`; `archives` lists them\n").as_bytes());
        };
        // Read from the archive itself: `img/flash.ron` names the part.
        let flash_ron = std::fs::read(&archive)
            .ok()
            .and_then(|bytes| crate::zip::entry(&bytes, "img/flash.ron"))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let Some(part) = part_of(&flash_ron) else {
            return out.write_all(format!("flash: {archive} does not say what part it is for\n").as_bytes());
        };
        if let Err(e) = std::fs::write("/dev/board", format!("part {part}\n")) {
            return out.write_all(format!("flash: the board would not become an {part}: {e}\n").as_bytes());
        }
        out.write_all(format!("the board is now an {part}, erased; flashing {archive}\n").as_bytes())?;
        let previous = self.archive.lock().unwrap_or_else(|e| e.into_inner()).replace(archive.clone());
        let flashed = self.run_humility(&["flash".to_string()], out)?;
        if flashed {
            let _ = std::fs::write("/dev/board", format!("image {archive}\n"));
        } else {
            *self.archive.lock().unwrap_or_else(|e| e.into_inner()) = previous;
        }
        Ok(())
    }
}

/// A line's words: split at whitespace, with `'…'` and `"…"` holding
/// spaces together, as a shell would for the arguments Humility takes.
fn split(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.get_or_insert_default().push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            (None, c) if c.is_whitespace() => words.extend(word.take()),
            (None, c) => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn an_archive_says_its_part_in_flash_ron() {
        assert_eq!(super::part_of("(\n    chip: Some(\"STM32G070RBTx\"),\n)\n"), Some("STM32G070RBTx"));
        assert_eq!(super::part_of("(chip: None)"), None);
    }

    #[test]
    fn a_line_splits_as_a_shell_would() {
        assert_eq!(split("humility tasks -s pong"), ["humility", "tasks", "-s", "pong"]);
        assert_eq!(split("  hiffy -c 'UserLeds.led_toggle' -a \"index=0\" "), ["hiffy", "-c", "UserLeds.led_toggle", "-a", "index=0"]);
        assert_eq!(split("say 'two words' ''"), ["say", "two words", ""]);
        assert!(split("   ").is_empty());
    }
}
