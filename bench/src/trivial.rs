//! The load generator's own limit.
//!
//! A ceiling is only an engine's if the load generator could have driven
//! more. So before any ceiling, oha on cores 8-15 is pointed at an endpoint
//! that does no work, served from the SUT cores, and ramped until it can no
//! longer keep the rate. No ceiling above a third of that is reported.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::host::{self, BoxSize};
use crate::log;
use crate::run::{Ctx, HeadroomRecord, Record, rate_step};
use crate::search;

pub const PORT: u16 = 7999;
const START_RPS: f64 = 10_000.0;
/// The smoke profile's two headroom steps.
const SMOKE_RPS: [f64; 2] = [10_000.0, 100_000.0];
const RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok";

/// Serves `200 ok` to every request on 127.0.0.1:`port`, one thread per
/// connection. The body of the hidden `trivial-server` subcommand.
pub fn serve(port: u16) -> Result<()> {
    let listener =
        TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("binding port {port}"))?;
    for stream in listener.incoming() {
        let stream = stream?;
        thread::spawn(move || handle(stream));
    }
    Ok(())
}

fn handle(mut s: TcpStream) {
    let _ = s.set_nodelay(true);
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match s.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        while let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            pending.drain(..end + 4);
            if s.write_all(RESPONSE).is_err() {
                return;
            }
        }
    }
}

/// Measures the highest rate oha sustains and sets the context's limit to a
/// third of it.
pub fn headroom(ctx: &mut Ctx) -> Result<HeadroomRecord> {
    host::set_sut(BoxSize::BIG)?;
    let exe = std::env::current_exe()?;
    let mut child = host::sut_command(&exe)
        .args(["trivial-server", "--port", &PORT.to_string()])
        .stdin(Stdio::null())
        .spawn()
        .context("starting the trivial server")?;
    host::register_child(child.id());
    let result = (|| {
        let url = format!("http://127.0.0.1:{PORT}/");
        let t0 = Instant::now();
        while ctx.http.get(&url).call().is_err() {
            if t0.elapsed() > Duration::from_secs(10) {
                bail!("the trivial server did not answer");
            }
            thread::sleep(Duration::from_millis(50));
        }
        let urls = ctx.out.join("urls/trivial.txt");
        std::fs::create_dir_all(urls.parent().unwrap())?;
        std::fs::write(&urls, format!("{url}\n"))?;
        let mut steps = Vec::new();
        let mut probe = |rate: f64| {
            let step = rate_step(ctx, BoxSize::BIG, &format!("headroom-q{rate}"), &urls, rate)?;
            let pass = step.pass;
            steps.push(step);
            Ok(pass)
        };
        let found = if ctx.profile.smoke {
            search::fixed(&SMOKE_RPS, None, &mut probe)?
        } else {
            search::ceiling(START_RPS, search::CEILING_TOLERANCE, None, &mut probe)?
        };
        Ok((found, steps))
    })();
    // The child is sudo; it relays SIGTERM to the server.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    host::unregister_child(child.id());
    let (found, steps) = result?;
    let Some(max) = found.ceiling else {
        bail!("the load generator could not sustain any rate against a trivial endpoint");
    };
    let max_rps = Some(max);
    let limit_rps = Some((max / 3.0).floor());
    ctx.load_limit = limit_rps;
    log(&format!(
        "load generator sustains {} req/s; ceilings above {} req/s will be marked load-generator bound",
        max_rps.map_or("no".into(), |m| m.to_string()),
        limit_rps.map_or("-".into(), |l| l.to_string())
    ));
    let rec = HeadroomRecord {
        max_rps,
        limit_rps,
        search: found,
        steps,
    };
    ctx.record(&Record::Headroom(rec.clone()))?;
    Ok(rec)
}
