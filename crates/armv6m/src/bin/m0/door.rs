//! `zaqaru-m0 door`: a socket that runs Humility for whoever asks.
//!
//! ```sh
//! zaqaru-m0 door --archive build-….zip --probe 127.0.0.1:3333 --listen 0.0.0.0:80
//! ```
//!
//! A line such as `tasks` (or `GET /tasks`, from a browser or curl) runs
//! `humility tasks` against the probe and sends back what it printed, one
//! command per connection, one at a time. `board` answers the board's own
//! state: under `serve` from the board thread, and on its own — beside a
//! board zaqaru's kernel runs — from `/proc/board`.

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
    println!("zaqaru-m0: humility on {listen}, against the probe on {probe} (send a command, e.g. \"tasks\")");
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

const HELP: &str = "\
This is an STM32G031 Nucleo running Hubris, emulated, with Humility on the
bench beside it. Send a Humility command, one per connection:

    tasks              the tasks, their state, and what they are waiting on
    tasks -s           ...and each task's stack
    map                the memory regions each task can reach
    manifest           what the archive says was built
    readvar -l         the kernel's and tasks' variables
    hiffy -l           the Idol interfaces hiffy can call
    hiffy -c UserLeds.led_toggle -a index=0
                       toggle the board's LED, through hiffy and the LED task
    board              the board itself: LED, time, faults

Over HTTP the words are the path: GET /tasks, GET /hiffy/-c/UserLeds.led_toggle/-a/index=0
";

pub(crate) fn accept_requests(listener: TcpListener, door: Arc<FrontDoor>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let door = door.clone();
        std::thread::spawn(move || {
            let _ = serve_request(stream, &door);
        });
    }
}

fn serve_request(stream: TcpStream, door: &FrontDoor) -> std::io::Result<()> {
    // No read timeout: zaqaru's kernel refuses SO_RCVTIMEO outright, and a
    // client that connects and says nothing costs one parked thread.
    let mut reader = BufReader::new(stream.try_clone()?).take(64 * 1024);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let (http, args) = parse_request(&line);
    if http {
        // The headers say nothing a command needs.
        let mut header = String::new();
        while reader.read_line(&mut header)? > 0 && header.trim_end() != "" {
            header.clear();
        }
    }

    let mut out = stream;
    if http {
        out.write_all(b"HTTP/1.0 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n")?;
    }
    match args.first().map(String::as_str) {
        None | Some("help") => out.write_all(HELP.as_bytes())?,
        Some("board" | "status") => match (door.status)() {
            Some(status) => out.write_all(status.as_bytes())?,
            None => out.write_all(b"no board answered\n")?,
        },
        Some(_) => {
            let _turn = door.one_at_a_time.lock().unwrap_or_else(|e| e.into_inner());
            out.write_all(format!("$ humility {}\n", args.join(" ")).as_bytes())?;
            let mut command = Command::new(&door.humility);
            command
                .args(&args)
                .env("HUMILITY_PROBE", format!("gdb:{}", door.gdb))
                .stdin(Stdio::null())
                .stdout(Stdio::from(OwnedFd::from(out.try_clone()?)))
                .stderr(Stdio::from(OwnedFd::from(out.try_clone()?)));
            if let Some(archive) = &door.archive {
                command.env("HUMILITY_ARCHIVE", archive);
            }
            match command.status() {
                Ok(status) if status.success() => {}
                Ok(status) => out.write_all(format!("({status})\n").as_bytes())?,
                Err(e) => out.write_all(format!("can't run {}: {e}\n", door.humility).as_bytes())?,
            }
        }
    }
    out.shutdown(Shutdown::Both)
}

/// A request's words: a line of them, or an HTTP request's path split at
/// its slashes. A leading `humility` is optional.
fn parse_request(line: &str) -> (bool, Vec<String>) {
    let line = line.trim_end();
    let (http, mut words): (bool, Vec<String>) = match line.strip_prefix("GET ") {
        Some(rest) => {
            let path = rest.split(' ').next().unwrap_or("/");
            let path = path.split('?').next().unwrap_or("");
            (true, path.split('/').filter(|w| !w.is_empty()).map(percent_decode).collect())
        }
        None => (false, line.split_whitespace().map(String::from).collect()),
    };
    if words.first().map(String::as_str) == Some("humility") {
        words.remove(0);
    }
    (http, words)
}

fn percent_decode(word: &str) -> String {
    let bytes = word.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%')
            .then(|| word.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match decoded {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
