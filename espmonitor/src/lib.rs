// Copyright 2021 Brian J. Tarricone <brian@tarricone.org>
//
// This file is part of ESPMonitor.
//
// ESPMonitor is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// ESPMonitor is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with ESPMonitor.  If not, see <https://www.gnu.org/licenses/>.

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use lazy_static::lazy_static;
use serial::{self, BaudRate, SerialPort, SystemPort};
use regex::Regex;
use std::{
    ffi::{OsString, OsStr},
    io::{self, Error as IoError, ErrorKind, Read, Write},
    path::Path,
    process::{Command, Stdio, exit},
    sync::{Arc, Mutex},
    thread::{self, sleep},
    time::{Duration, Instant},
};

const DEFAULT_BAUD_RATE: BaudRate = BaudRate::Baud115200;
const UNFINISHED_LINE_TIMEOUT: Duration = Duration::from_secs(5);

lazy_static! {
    static ref FUNC_ADDR_RE: Regex = Regex::new(r"0x4[0-9a-f]{7}")
        .expect("Failed to parse program address regex");
    static ref ADDR2LINE_RE: Regex = Regex::new(r"^0x[0-9a-f]+:\s+([^ ]+)\s+at\s+(\?\?|[0-9]+):(\?|[0-9]+)")
        .expect("Failed to parse addr2line output regex");
}

macro_rules! rprintln {
    () => (print!("\r\n"));
    ($fmt:literal) => (print!(concat!($fmt, "\r\n")));
    ($fmt:literal, $($arg:tt)+) => (print!(concat!($fmt, "\r\n"), $($arg)*));
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Framework {
    Baremetal,
    EspIdf,
}

impl Framework {
    pub fn from_target<S: AsRef<str>>(target: S) -> Result<Self, IoError> {
        let target = target.as_ref();
        if target.ends_with("-espidf") {
            Ok(Framework::EspIdf)
        } else if target.ends_with("-none-elf") {
            Ok(Framework::Baremetal)
        } else {
            Err(IoError::new(ErrorKind::InvalidInput, format!("Can't figure out framework from target '{}'", target)))
        }
    }
}

impl std::convert::TryFrom<&str> for Framework {
    type Error = IoError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "baremetal" => Ok(Framework::Baremetal),
            "esp-idf" | "espidf" => Ok(Framework::EspIdf),
            _ => Err(IoError::new(ErrorKind::InvalidInput, format!("'{}' is not a valid framework", value))),
        }
    }
}

impl Default for Framework {
    fn default() -> Self {
        Framework::Baremetal
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Chip {
    ESP32,
    ESP32S2,
    ESP8266,
}

impl Chip {
    pub fn from_target<S: AsRef<str>>(target: S) -> Result<Chip, IoError> {
        let target = target.as_ref();
        if target.contains("-esp32-") {
            Ok(Chip::ESP32)
        } else if target.contains("-esp32s2-") {
            Ok(Chip::ESP32S2)
        } else if target.contains("-esp8266-") {
            Ok(Chip::ESP8266)
        } else {
            Err(IoError::new(ErrorKind::InvalidInput, format!("Can't figure out chip from target '{}'", target)))
        }
    }
}

impl Chip {
    pub fn target(&self, framework: Framework) -> String {
        let mut target = String::from("xtensa-");
        target.push_str(match self {
            Chip::ESP32 => "esp32-",
            Chip::ESP32S2 => "esp32s2-",
            Chip::ESP8266 => "esp8266-",
        });
        target.push_str(match framework {
            Framework::Baremetal => "none-elf",
            Framework::EspIdf=> "espidf",
        });
        target
    }

    pub fn tool_prefix(&self) -> &'static str {
        match self {
            Chip::ESP32 => "xtensa-esp32-elf-",
            Chip::ESP32S2 => "xtensa-esp32s2-elf-",
            Chip::ESP8266 => "xtensa-esp8266-elf-",
        }
    }
}

impl std::convert::TryFrom<&str> for Chip {
    type Error = IoError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "esp32" => Ok(Chip::ESP32),
            "esp8266" => Ok(Chip::ESP8266),
            _ => Err(IoError::new(ErrorKind::InvalidInput, format!("'{}' is not a valid chip", value))),
        }
    }
}

impl Default for Chip {
    fn default() -> Self {
        Chip::ESP32
    }
}

#[derive(Debug)]
pub struct AppArgs {
    pub serial: String,
    pub chip: Chip,
    pub framework: Framework,
    pub speed: Option<usize>,
    pub reset: bool,
    pub bin: Option<OsString>,
}

struct SerialState {
    unfinished_line: String,
    last_unfinished_line_at: Instant,
    bin: Option<OsString>,
    tool_prefix: &'static str,
}

#[cfg(unix)]
pub fn run(args: AppArgs) -> Result<(), Box<dyn std::error::Error>> {
    use nix::{sys::wait::{WaitStatus, waitpid}, unistd::{ForkResult, fork}};

    enable_raw_mode()?;

    match unsafe { fork() } {
        Err(err) => Err(err.into()),
        Ok(ForkResult::Parent { child }) => loop {
            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, status)) => {
                    disable_raw_mode()?;
                    exit(status);
                },
                Ok(WaitStatus::Signaled(_, _, _)) => {
                    disable_raw_mode()?;
                    exit(255);
                },
                _ => (),
            }
        }
        Ok(ForkResult::Child) => run_child(args),
    }
}

#[cfg(windows)]
pub fn run(args: AppArgs) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let result = run_child(args);
    disable_raw_mode()?;
    result
}

fn run_child(mut args: AppArgs) -> Result<(), Box<dyn std::error::Error>> {
    rprintln!("ESPMonitor {}", env!("CARGO_PKG_VERSION"));
    rprintln!();
    rprintln!("Commands:");
    rprintln!("    CTRL+R    Reset chip");
    rprintln!("    CTRL+C    Exit");
    rprintln!();

    let speed = args.speed.map(BaudRate::from_speed).unwrap_or(DEFAULT_BAUD_RATE);
    rprintln!("Opening {} with speed {}", args.serial, speed.speed());

    let mut dev = serial::open(&args.serial)?;
    dev.set_timeout(Duration::from_millis(200))?;
    dev.reconfigure(&|settings| {
        settings.set_baud_rate(speed)
    })?;

    if let Some(bin) = args.bin.as_ref() {
        if Path::new(bin).exists() {
            rprintln!("Using {} as flash image", bin.to_string_lossy());
        } else {
            rprintln!("WARNING: Flash image {} does not exist (you may need to build it)", bin.to_string_lossy());
        }
    }

    if args.reset {
        reset_chip(&mut dev)?;
    }

    let dev = Arc::new(Mutex::new(dev));

    let _input_thread = {
        let dev = Arc::clone(&dev);
        thread::spawn(||
            if stdin_thread_fn(dev).is_err() {
                exit(1);
            }
        )
    };

    let mut serial_state = SerialState {
        unfinished_line: String::new(),
        last_unfinished_line_at: Instant::now(),
        bin: args.bin.take(),
        tool_prefix: args.chip.tool_prefix(),
    };

    let mut buf = [0u8; 1024];
    loop {
        let bytes = match dev.lock().unwrap().read(&mut buf) {
            Ok(bytes) if bytes > 0 => Some(bytes),
            Ok(_) => None,
            Err(err) if err.kind() == ErrorKind::TimedOut => None,
            Err(err) if err.kind() == ErrorKind::WouldBlock => None,
            Err(err) => break Err(err.into()),
        };

        if let Some(bytes) = bytes {
            handle_serial(&mut serial_state, &buf[0..bytes])?;
        } else {
            // Give the stdin thread a chance to wake up and lock if it wants to
            sleep(Duration::from_millis(25));
        }
    }
}

fn reset_chip(dev: &mut SystemPort) -> io::Result<()> {
    print!("Resetting device... ");
    std::io::stdout().flush()?;
    dev.set_dtr(false)?;
    dev.set_rts(true)?;
    dev.set_rts(false)?;
    rprintln!("done");
    Ok(())
}

fn stdin_thread_fn(dev: Arc<Mutex<SystemPort>>) -> io::Result<()> {
    loop {
        if event::poll(Duration::from_millis(250))? {
            match event::read() {
                Ok(Event::Key(key_event)) => {
                    if key_event.modifiers == KeyModifiers::CONTROL {
                        match key_event.code {
                            KeyCode::Char('r') => {
                                let mut dev = dev.lock().unwrap();
                                reset_chip(&mut dev)?;
                            },
                            KeyCode::Char('c') => exit(0),
                            _ => (),
                        }
                    }
                },
                Ok(_) => (),
                Err(err) => {
                    rprintln!("Error reading from terminal: {}", err);
                    break Err(err);
                },
            }
        }
    }
}

fn handle_serial(state: &mut SerialState, buf: &[u8]) -> io::Result<()> {
    let data = String::from_utf8_lossy(buf);
    let mut lines = data.split('\n').collect::<Vec<&str>>();

    let new_unfinished_line =
        if data.ends_with('\n') {
            None
        } else {
            lines.pop()
        };

    for line in lines {
        let full_line =
            if !state.unfinished_line.is_empty() {
                state.unfinished_line.push_str(line);
                state.unfinished_line.as_str()
            } else {
                line
            };

        if !full_line.is_empty() {
            let processed_line = process_line(state, full_line);
            rprintln!("{}", processed_line);
            state.unfinished_line.clear();
        }
    }

    if let Some(nel) = new_unfinished_line {
        state.unfinished_line.push_str(nel);
        state.last_unfinished_line_at = Instant::now();
    } else if !state.unfinished_line.is_empty() && state.last_unfinished_line_at.elapsed() > UNFINISHED_LINE_TIMEOUT {
        let processed_line = process_line(state, &state.unfinished_line);
        rprintln!("{}", processed_line);
        state.unfinished_line.clear();
    }

    Ok(())
}

fn process_line(state: &SerialState, line: &str) -> String {
    let mut updated_line = line.to_string();

    if let Some(bin) = state.bin.as_ref() {
        for mat in FUNC_ADDR_RE.find_iter(line) {
            let cmd = format!("{}addr2line", state.tool_prefix);
            if let Some(output) = Command::new(&cmd)
                .args(&[OsStr::new("-pfiaCe"), bin, OsStr::new(mat.as_str())])
                .stdout(Stdio::piped())
                .output()
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
            {
                if let Some(caps) = ADDR2LINE_RE.captures(&output) {
                    let name = format!("{} [{}:{}:{}]", mat.as_str().to_string(), caps[1].to_string(), caps[2].to_string(), caps[3].to_string());
                    updated_line = updated_line.replace(mat.as_str(), &name);
                }
            }
        }
    }

    updated_line
}
