//! `zaqaru-m0 door`: a terminal session with Humility on the bench.
//!
//! ```sh
//! zaqaru-m0 door --archive build-….zip --probe 127.0.0.1:3333 --listen 0.0.0.0:80
//! ```
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
    println!("zaqaru-m0: a humility session on {listen}, against the probe on {probe}");
    let status = || std::fs::read_to_string("/proc/board").ok();
    accept_requests(listener, Arc::new(FrontDoor::new(humility, archive, probe, Box::new(status))));
    ExitCode::SUCCESS
}

pub(crate) struct FrontDoor {
    humility: String,
    archive: Option<String>,
    /// The probe, as Humility is to reach it.
    gdb: String,
    /// The probe has one debugger at a time, so Humility runs one at a time.
    one_at_a_time: Mutex<()>,
    /// The board's state, for `board`; `None` if there is none to ask.
    status: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

impl FrontDoor {
    pub(crate) fn new(
        humility: String,
        archive: Option<String>,
        gdb: String,
        status: Box<dyn Fn() -> Option<String> + Send + Sync>,
    ) -> FrontDoor {
        FrontDoor { humility, archive, gdb, one_at_a_time: Mutex::new(()), status }
    }
}

const BANNER: &str = "\
An STM32G031 Nucleo running Hubris, emulated in zaqaru's kernel, with its
debug probe on 127.0.0.1:3333 and Humility attached through it.
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
    board                   the board itself: state, time, LED, faults
    exit                    end the session
";

const PROMPT: &[u8] = b"$ ";

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
    let mut lines = BufReader::new(stream.try_clone()?).take(1 << 20);
    let mut out = stream;
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
        let _turn = self.one_at_a_time.lock().unwrap_or_else(|e| e.into_inner());
        let mut command = Command::new(&self.humility);
        command
            .args(args)
            .env("HUMILITY_PROBE", format!("gdb:{}", self.gdb))
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(out.try_clone()?)))
            .stderr(Stdio::from(OwnedFd::from(out.try_clone()?)));
        if let Some(archive) = &self.archive {
            command.env("HUMILITY_ARCHIVE", archive);
        }
        let mut out = out;
        match command.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => out.write_all(format!("({status})\n").as_bytes()),
            Err(e) => out.write_all(format!("can't run {}: {e}\n", self.humility).as_bytes()),
        }
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
    fn a_line_splits_as_a_shell_would() {
        assert_eq!(split("humility tasks -s pong"), ["humility", "tasks", "-s", "pong"]);
        assert_eq!(split("  hiffy -c 'UserLeds.led_toggle' -a \"index=0\" "), ["hiffy", "-c", "UserLeds.led_toggle", "-a", "index=0"]);
        assert_eq!(split("say 'two words' ''"), ["say", "two words", ""]);
        assert!(split("   ").is_empty());
    }
}
