//! Experiment: can doctor show the bar's real glyphs in the terminal?
//!
//! Probes for the kitty graphics protocol the way kitty documents it: send a
//! graphics query followed by a Primary Device Attributes (DA) request. Every
//! terminal answers DA; one that speaks the graphics protocol answers the
//! query first. DA is the sentinel, so a terminal that ignores the query
//! never makes us wait for the timeout.
//!
//! ```text
//!   us  ──► ESC _G i=31,a=q,… ESC \  ESC [ c
//!   tty ◄── ESC _G i=31;OK ESC \      (only with kitty graphics)
//!   tty ◄── ESC [ ? 62 ; 4 ; … c      (always; a `4` means sixel)
//! ```

use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::time::{Duration, Instant};

use base64::Engine as _;

/// A 1×1 query-only image (`a=q`) and the DA request behind it.
const QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c";
const KITTY_OK: &[u8] = b"\x1b_Gi=31;OK";
const DA_START: &[u8] = b"\x1b[?";
/// DA attribute that advertises sixel graphics.
const DA_SIXEL: &str = "4";
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);
const SQUARE_SIDE: usize = 32;

/// What the terminal said, and what it means.
pub struct Probe {
    pub reply: Vec<u8>,
    pub kitty: bool,
    pub sixel: bool,
}

impl Probe {
    fn from_reply(reply: Vec<u8>) -> Self {
        let kitty = find(&reply, KITTY_OK).is_some();
        let sixel = da_params(&reply).iter().any(|p| p == DA_SIXEL);
        Self {
            reply,
            kitty,
            sixel,
        }
    }
}

/// Raw mode for the duration of the probe: no line buffering, no echo of the
/// terminal's reply. Restores the previous mode when dropped.
struct RawMode {
    fd: i32,
    saved: libc::termios,
}

impl RawMode {
    fn enter(fd: i32) -> std::io::Result<Self> {
        // SAFETY: tcgetattr only writes into the termios we pass it.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: fd is an open terminal and raw is a valid termios.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring the termios read from this same fd.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

/// Ask the controlling terminal. Fails when there is none (piped, cron, CI).
pub fn probe() -> std::io::Result<Probe> {
    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let fd = tty.as_raw_fd();
    let _raw = RawMode::enter(fd)?;

    tty.write_all(QUERY)?;
    tty.flush()?;

    // Read until the DA reply is complete, or give up at the deadline.
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut reply = Vec::new();
    while !da_complete(&reply) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !readable(fd, left)? {
            break;
        }

        let mut buf = [0u8; 256];
        let n = tty.read(&mut buf)?;
        if n == 0 {
            break;
        }
        reply.extend_from_slice(&buf[..n]);
    }
    Ok(Probe::from_reply(reply))
}

/// Whether `fd` has input within `timeout`.
fn readable(fd: i32, timeout: Duration) -> std::io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one valid pollfd.
    match unsafe { libc::poll(&mut pfd, 1, ms) } {
        -1 => Err(std::io::Error::last_os_error()),
        0 => Ok(false),
        _ => Ok(true),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether the reply holds a full DA response: `ESC [ ? … c`.
fn da_complete(reply: &[u8]) -> bool {
    find(reply, DA_START).is_some_and(|start| reply[start..].contains(&b'c'))
}

/// The DA attributes: `ESC [ ? 62 ; 4 ; 22 c` → `["62", "4", "22"]`.
fn da_params(reply: &[u8]) -> Vec<String> {
    let Some(start) = find(reply, DA_START) else {
        return Vec::new();
    };
    let body = &reply[start + DA_START.len()..];
    let Some(end) = body.iter().position(|&b| b == b'c') else {
        return Vec::new();
    };
    String::from_utf8_lossy(&body[..end])
        .split(';')
        .map(str::to_string)
        .collect()
}

/// The reply with escapes made visible: `ESC` as `\e`, others as `\xNN`.
fn visible(reply: &[u8]) -> String {
    reply
        .iter()
        .map(|&b| match b {
            0x1b => "\\e".to_string(),
            0x20..=0x7e => (b as char).to_string(),
            _ => format!("\\x{b:02x}"),
        })
        .collect()
}

/// A gradient square sent as raw RGB pixels: proof that images get through.
fn draw_square() {
    let mut rgb = Vec::with_capacity(SQUARE_SIDE * SQUARE_SIDE * 3);
    for y in 0..SQUARE_SIDE {
        for x in 0..SQUARE_SIDE {
            let scale = |v: usize| (v * 255 / (SQUARE_SIDE - 1)) as u8;
            rgb.extend([scale(x), scale(y), 255 - scale(x)]);
        }
    }

    let payload = base64::engine::general_purpose::STANDARD.encode(rgb);
    print!("\x1b_Ga=T,f=24,s={SQUARE_SIDE},v={SQUARE_SIDE};{payload}\x1b\\");
}

/// `--doctor-probe-terminal`: report what the terminal supports.
pub fn run_probe() {
    let env = |var: &str| std::env::var(var).unwrap_or_else(|_| "-".into());
    println!(
        "env: TERM={} TERM_PROGRAM={} TMUX={}",
        env("TERM"),
        env("TERM_PROGRAM"),
        env("TMUX")
    );

    let probe = match probe() {
        Ok(probe) => probe,
        Err(err) => {
            println!("no terminal to probe ({err}): doctor would stay text-only");
            return;
        }
    };

    let yes_no = |b: bool| if b { "yes" } else { "no" };
    println!("reply: {}", visible(&probe.reply));
    println!("DA attributes: {}", da_params(&probe.reply).join(";"));
    println!("kitty graphics: {}", yes_no(probe.kitty));
    println!("sixel: {}", yes_no(probe.sixel));

    if probe.kitty {
        print!("test image, should be a {SQUARE_SIDE}px gradient square: ");
        draw_square();
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_reply_before_da_means_kitty_graphics() {
        let probe = Probe::from_reply(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c".to_vec());
        assert!(probe.kitty);
        assert!(!probe.sixel);
    }

    #[test]
    fn da_alone_means_no_kitty_graphics() {
        // e.g. alacritty: answers DA only
        let probe = Probe::from_reply(b"\x1b[?6c".to_vec());
        assert!(!probe.kitty);
    }

    #[test]
    fn da_attribute_4_means_sixel() {
        let probe = Probe::from_reply(b"\x1b[?62;4;22c".to_vec());
        assert!(probe.sixel);
        assert_eq!(da_params(&probe.reply), ["62", "4", "22"]);
    }

    #[test]
    fn a_kitty_error_reply_is_not_support() {
        let probe = Probe::from_reply(b"\x1b_Gi=31;EINVAL\x1b\\\x1b[?62c".to_vec());
        assert!(!probe.kitty);
    }

    #[test]
    fn da_is_complete_only_once_terminated() {
        assert!(!da_complete(b"\x1b[?62;4"));
        assert!(da_complete(b"\x1b[?62;4c"));
    }
}
