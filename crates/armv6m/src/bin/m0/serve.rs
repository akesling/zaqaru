//! `zaqaru-m0 serve`: the emulated board on a bench, with a probe on it.
//!
//! ```sh
//! zaqaru-m0 serve final.bin --archive build-….zip --gdb 127.0.0.1:3333 --listen 0.0.0.0:80
//! ```
//!
//! Three things run at once, each on its own thread:
//!
//! - **The board.** One thread owns the [`Machine`] and is the only thing
//!   that touches it; the others send it messages. It runs Hubris against
//!   the wall clock — 16 MHz of simulated time per second of real time,
//!   not faster — so that a timer the firmware sets and a timeout a
//!   debugger waits on mean the same thing. A board that cannot keep up
//!   loses the time rather than racing to make it up, and a halted board's
//!   time stands still, as SysTick's does under a debugger.
//! - **The probe**, a GDB server ([`armv6m::gdb`]). GDB can attach to it,
//!   and so can Humility, with `--probe gdb:HOST:PORT`.
//! - **The front door**, a socket that runs Humility for whoever asks: a
//!   line such as `tasks` (or `GET /tasks`, from a browser or curl) runs
//!   `humility tasks` against the probe and sends back what it printed. A
//!   line of `board` answers the board's own state instead.
//!
//! The board's state — LED, time, faults — is also written to `--board`
//! whenever it changes, as a file anything can read.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use armv6m::gdb::{self, Debugger, Response};
use armv6m::scs::exception;
use armv6m::{Event, Machine, Outcome};

use crate::{CYCLES_PER_MS, describe, ms_at, usage};

/// How long the board sleeps between catching up with the wall clock when
/// nothing asks anything of it. A message wakes it at once, so this is not
/// a latency; it is how often an idle board pays for a wake-up, which under
/// an interpreter is the clock reads and the futex, a hundred times a
/// second for nothing at 10 ms.
const SLICE: Duration = Duration::from_millis(100);

/// How far behind the wall clock the board may fall before it gives the
/// time up.
const MAX_BEHIND_MS: u64 = 200;

struct Options {
    image: String,
    gdb: String,
    listen: String,
    archive: Option<String>,
    humility: String,
    board: Option<String>,
}

enum Message {
    Connected { id: usize, stream: TcpStream },
    Packet { id: usize, payload: String },
    Interrupt { id: usize },
    Closed { id: usize },
    Status(Sender<String>),
}

pub fn main(args: Vec<String>) -> ExitCode {
    let mut options = Options {
        image: String::new(),
        gdb: "127.0.0.1:3333".into(),
        listen: "0.0.0.0:80".into(),
        archive: None,
        humility: "humility".into(),
        board: None,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "--gdb" => &mut options.gdb,
            "--listen" => &mut options.listen,
            "--humility" => &mut options.humility,
            "--archive" => options.archive.insert(String::new()),
            "--board" => options.board.insert(String::new()),
            _ if options.image.is_empty() && !arg.starts_with('-') => {
                options.image = arg;
                continue;
            }
            _ => return usage(),
        };
        match args.next() {
            Some(value) => *slot = value,
            None => return usage(),
        }
    }
    if options.image.is_empty() {
        return usage();
    }

    let machine = match std::fs::read(&options.image).map_err(|e| e.to_string()).and_then(|b| Machine::stm32g031(&b)) {
        Ok(machine) => machine,
        Err(e) => {
            eprintln!("zaqaru-m0: {}: {e}", options.image);
            return ExitCode::FAILURE;
        }
    };
    let bind = |address: &str| {
        TcpListener::bind(address).map_err(|e| {
            eprintln!("zaqaru-m0: can't listen on {address}: {e}");
            ExitCode::FAILURE
        })
    };
    let probe = match bind(&options.gdb) {
        Ok(listener) => listener,
        Err(code) => return code,
    };
    let door = match bind(&options.listen) {
        Ok(listener) => listener,
        Err(code) => return code,
    };

    println!("zaqaru-m0: STM32G031 running {}", options.image);
    println!("zaqaru-m0: GDB server on {}", options.gdb);
    println!("zaqaru-m0: humility on {} (send a command, e.g. \"tasks\")", options.listen);

    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        std::thread::spawn(move || accept_debuggers(probe, tx));
    }
    {
        // Humility connects to the probe locally whatever address it
        // listens on.
        let gdb = options.gdb.replace("0.0.0.0", "127.0.0.1");
        let door_state =
            Arc::new(FrontDoor { humility: options.humility.clone(), archive: options.archive.clone(), gdb, one_at_a_time: Mutex::new(()), tx });
        std::thread::spawn(move || accept_requests(door, door_state));
    }

    Board::new(machine, options).run(rx);
    ExitCode::SUCCESS
}

struct Board {
    machine: Machine,
    debugger: Debugger,
    client: Option<(usize, TcpStream)>,
    /// The instant simulated time zero corresponds to. Moved forward
    /// whenever the board halts or falls behind, so that simulated time is
    /// always time the board actually ran.
    origin: Instant,
    /// When the board was powered on, by the wall clock.
    powered: Instant,
    image: String,
    board_file: Option<String>,
    led: Option<bool>,
    faults: u64,
}

impl Board {
    fn new(machine: Machine, options: Options) -> Board {
        Board {
            machine,
            debugger: Debugger::default(),
            client: None,
            origin: Instant::now(),
            powered: Instant::now(),
            image: options.image,
            board_file: options.board,
            led: None,
            faults: 0,
        }
    }

    fn run(mut self, rx: Receiver<Message>) {
        self.publish();
        loop {
            let halted = self.debugger.halted;
            let wait = if halted { Duration::from_secs(3600) } else { SLICE };
            match rx.recv_timeout(wait) {
                Ok(message) => {
                    self.handle(message);
                    while let Ok(message) = rx.try_recv() {
                        self.handle(message);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if halted && !self.debugger.halted {
                self.rebase();
            }
            if !self.debugger.halted {
                self.advance();
            }
        }
    }

    /// Restarts the clock from where the board is now.
    fn rebase(&mut self) {
        let ran = Duration::from_micros(self.machine.cycles * 1000 / CYCLES_PER_MS);
        self.origin = Instant::now().checked_sub(ran).unwrap_or_else(Instant::now);
    }

    fn advance(&mut self) {
        let due = self.origin.elapsed().as_micros() as u64 * CYCLES_PER_MS / 1000;
        let behind = due.saturating_sub(self.machine.cycles);
        let budget = behind.min(MAX_BEHIND_MS * CYCLES_PER_MS);
        let outcome = self.machine.run(budget);
        if behind > MAX_BEHIND_MS * CYCLES_PER_MS {
            self.rebase();
        }
        self.report();
        match outcome {
            Outcome::Budget | Outcome::Asleep => {}
            Outcome::SystemReset => {
                println!("{:>10.3} ms  system reset", ms_at(self.machine.cycles));
                if let Err(e) = self.machine.reset() {
                    println!("zaqaru-m0: reset failed: {e}");
                }
            }
            outcome => {
                println!("{:>10.3} ms  halted: {outcome:?}", ms_at(self.machine.cycles));
                if let Some(stop) = self.debugger.stopped(outcome) {
                    self.send(&stop);
                }
                self.publish();
            }
        }
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Connected { id, stream } => {
                if let Some((_, old)) = self.client.replace((id, stream)) {
                    let _ = old.shutdown(Shutdown::Both);
                }
                // Whatever the last debugger left, the next one finds the
                // board running.
                self.release();
            }
            Message::Packet { id, payload } if self.is_client(id) => {
                let response = self.debugger.packet(&mut self.machine, &payload);
                match response {
                    Response::Reply(reply) => self.write(&format!("+{}", gdb::frame(&reply))),
                    Response::Resumed => self.write("+"),
                    Response::Detach(reply) => {
                        self.write(&format!("+{}", gdb::frame(&reply)));
                        if let Some((_, stream)) = self.client.take() {
                            let _ = stream.shutdown(Shutdown::Both);
                        }
                    }
                }
                // A debugger's write can change a pin.
                self.report();
            }
            Message::Interrupt { id } if self.is_client(id) && !self.debugger.halted => {
                let stop = self.debugger.interrupt();
                self.send(&stop);
            }
            Message::Closed { id } if self.is_client(id) => {
                self.client = None;
                self.release();
            }
            Message::Status(reply) => {
                let _ = reply.send(self.status());
            }
            _ => {}
        }
    }

    fn is_client(&self, id: usize) -> bool {
        self.client.as_ref().is_some_and(|(current, _)| *current == id)
    }

    fn release(&mut self) {
        self.machine.breakpoints.clear();
        if self.debugger.halted {
            self.debugger.halted = false;
            self.machine.resume();
        }
    }

    fn send(&mut self, payload: &str) {
        self.write(&gdb::frame(payload));
    }

    fn write(&mut self, bytes: &str) {
        if let Some((_, stream)) = &mut self.client
            && stream.write_all(bytes.as_bytes()).is_err() {
                self.client = None;
            }
    }

    /// Prints what the board did since last asked, and republishes its
    /// state if the LED changed or something faulted.
    fn report(&mut self) {
        let mut changed = false;
        for event in self.machine.events.drain(..) {
            println!("{}", describe(&event));
            match event {
                Event::Pin { edge, .. } if edge.port == 2 && (edge.before ^ edge.after) & 1 << 6 != 0 => {
                    // PC6 drives the Nucleo's LED, active low.
                    self.led = Some(edge.after & 1 << 6 == 0);
                    changed = true;
                }
                Event::Fault { .. } => {
                    self.faults += 1;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            self.publish();
        }
    }

    fn publish(&self) {
        if let Some(path) = &self.board_file
            && let Err(e) = std::fs::write(path, self.status()) {
                println!("zaqaru-m0: can't write {path}: {e}");
            }
    }

    fn status(&self) -> String {
        let m = &self.machine;
        let state = if self.debugger.halted {
            format!("halted at pc=0x{:08x}", m.cpu.pc)
        } else {
            "running".into()
        };
        let led = match self.led {
            Some(true) => "on",
            Some(false) => "off",
            None => "never driven",
        };
        let taken: Vec<String> = m
            .taken
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count > 0)
            .map(|(n, count)| format!("{} {count}", exception::name(n as u32)))
            .collect();
        format!(
            "board       STM32G031 Nucleo (emulated Cortex-M0+)\n\
             image       {}\n\
             state       {state}\n\
             time        {:.3} s simulated in {:.3} s powered on, {} instructions\n\
             LED (PC6)   {led}\n\
             faults      {}\n\
             exceptions  {}\n",
            self.image,
            ms_at(m.cycles) / 1000.0,
            self.powered.elapsed().as_secs_f64(),
            m.retired,
            self.faults,
            taken.join(", "),
        )
    }
}

fn accept_debuggers(listener: TcpListener, tx: Sender<Message>) {
    for (id, stream) in listener.incoming().enumerate() {
        let Ok(stream) = stream else { continue };
        let _ = stream.set_nodelay(true);
        let Ok(writer) = stream.try_clone() else { continue };
        if tx.send(Message::Connected { id, stream: writer }).is_err() {
            return;
        }
        let tx = tx.clone();
        std::thread::spawn(move || read_packets(id, stream, tx));
    }
}

/// Splits a debugger's byte stream into packets and interrupts. Acks are
/// dropped; the board acknowledges each packet as it answers it.
fn read_packets(id: usize, stream: TcpStream, tx: Sender<Message>) {
    let mut bytes = BufReader::new(stream).bytes();
    let mut next = move || bytes.next().and_then(Result::ok);
    while let Some(byte) = next() {
        let message = match byte {
            b'$' => {
                let mut payload = Vec::new();
                loop {
                    match next() {
                        Some(b'#') => break,
                        Some(b) => payload.push(b),
                        None => return closed(id, &tx),
                    }
                }
                // The checksum: TCP has already checked the bytes.
                let _ = (next(), next());
                Message::Packet { id, payload: String::from_utf8_lossy(&payload).into_owned() }
            }
            0x03 => Message::Interrupt { id },
            _ => continue,
        };
        if tx.send(message).is_err() {
            return;
        }
    }
    closed(id, &tx);
}

fn closed(id: usize, tx: &Sender<Message>) {
    let _ = tx.send(Message::Closed { id });
}

struct FrontDoor {
    humility: String,
    archive: Option<String>,
    gdb: String,
    /// The probe has one debugger at a time, so Humility runs one at a time.
    one_at_a_time: Mutex<()>,
    tx: Sender<Message>,
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

fn accept_requests(listener: TcpListener, door: Arc<FrontDoor>) {
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
        Some("board" | "status") => {
            let (reply, answer) = mpsc::channel();
            let _ = door.tx.send(Message::Status(reply));
            if let Ok(status) = answer.recv() {
                out.write_all(status.as_bytes())?;
            }
        }
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
