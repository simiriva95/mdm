//! Modalità native messaging host: Chrome ci lancia, noi inoltriamo il job
//! all'istanza app via TCP localhost (avviandola se serve) e rispondiamo.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::server::PORT;

pub fn run() {
    let mut stdin = std::io::stdin().lock();
    loop {
        let mut len4 = [0u8; 4];
        if stdin.read_exact(&mut len4).is_err() {
            return; // Chrome ha chiuso lo stream
        }
        let len = u32::from_le_bytes(len4) as usize;
        if len == 0 || len > 1 << 20 {
            return;
        }
        let mut buf = vec![0u8; len];
        if stdin.read_exact(&mut buf).is_err() {
            return;
        }

        let reply = match forward(&buf) {
            Ok(()) => br#"{"ok":true}"#.to_vec(),
            Err(e) => format!(r#"{{"ok":false,"error":"{}"}}"#, e.to_string().replace('"', "'")).into_bytes(),
        };
        let mut out = std::io::stdout().lock();
        if out.write_all(&(reply.len() as u32).to_le_bytes()).is_err()
            || out.write_all(&reply).is_err()
            || out.flush().is_err()
        {
            return;
        }
    }
}

fn forward(job: &[u8]) -> anyhow::Result<()> {
    let mut stream = connect_or_spawn()?;
    stream.write_all(job)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    anyhow::ensure!(line.contains("ok"), "risposta app inattesa: {line}");
    Ok(())
}

fn connect_or_spawn() -> anyhow::Result<TcpStream> {
    let addr = ("127.0.0.1", PORT);
    if let Ok(s) = TcpStream::connect(addr) {
        return Ok(s);
    }

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    let spawned = {
        use std::os::windows::process::CommandExt;
        // Chrome mette l'host in un job object che ucciderebbe i figli:
        // CREATE_BREAKAWAY_FROM_JOB (0x0100_0000) + DETACHED_PROCESS (0x8)
        cmd.creation_flags(0x0100_0008).spawn().or_else(|_| Command::new(&exe)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn())
    };
    #[cfg(not(windows))]
    let spawned = cmd.spawn();
    spawned?;

    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(s) = TcpStream::connect(addr) {
            return Ok(s);
        }
    }
    anyhow::bail!("app non raggiungibile su porta {PORT}")
}
