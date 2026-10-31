//! Talking to the grove server: connect (starting the server if needed), send requests,
//! receive replies.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::wire::{Reply, Request, SOCKET};

#[derive(Clone)]
pub struct Remote {
    tx: mpsc::UnboundedSender<Request>,
}

impl Remote {
    pub fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }

    /// A remote whose requests go nowhere (for tests).
    #[cfg(test)]
    pub fn detached() -> (Self, mpsc::UnboundedReceiver<Request>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Incoming {
    Reply(Reply),
    /// The connection dropped.
    Disconnected,
}

/// Connects to the server. With `autostart`, starts one if none is running; otherwise
/// waits up to `wait` for one to come up (e.g. while it restarts after an upgrade).
pub async fn connect(
    home: &Path,
    autostart: bool,
    wait: Duration,
) -> Result<(Remote, mpsc::UnboundedReceiver<Incoming>)> {
    let socket = home.join(SOCKET);
    let deadline = tokio::time::Instant::now() + wait;
    let mut started = false;
    let stream = loop {
        match UnixStream::connect(&socket).await {
            Ok(s) => break s,
            Err(e) => {
                if autostart && !started {
                    start_server(home)?;
                    started = true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(e).with_context(|| {
                        format!("connecting to the grove server at {}", socket.display())
                    });
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    let (rd, mut wr) = stream.into_split();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Request>();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(req) = req_rx.recv().await {
            let Ok(mut l) = serde_json::to_vec(&req) else {
                continue;
            };
            l.push(b'\n');
            if wr.write_all(&l).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            match serde_json::from_str::<Reply>(&l) {
                Ok(reply) => {
                    if in_tx.send(Incoming::Reply(reply)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = in_tx.send(Incoming::Reply(Reply::Error {
                        message: format!("bad reply from server: {e}"),
                    }));
                }
            }
        }
        let _ = in_tx.send(Incoming::Disconnected);
    });
    let remote = Remote { tx: req_tx };
    remote.send(Request::Hello);
    Ok((remote, in_rx))
}

/// Starts `grove server` detached from this terminal: its own session, no stdin, output
/// to ~/.grove/server.log.
fn start_server(home: &Path) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("server.log"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("server")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // It must not belong to the UI's process: no inherited upgrade marker.
        .env_remove("GROVE_UPGRADED");
    // Safety: setsid only detaches the child from our terminal and session; it's
    // async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting the grove server")?;
    Ok(())
}

/// Sends one request and waits for the first reply `want` accepts (or an error).
pub async fn request(
    home: &Path,
    autostart: bool,
    req: Request,
    want: impl Fn(&Reply) -> bool,
) -> Result<Reply> {
    let (remote, mut rx) = connect(home, autostart, Duration::from_secs(5)).await?;
    remote.send(req);
    let wait = async {
        while let Some(incoming) = rx.recv().await {
            match incoming {
                Incoming::Reply(Reply::Error { message }) => bail!(message),
                Incoming::Reply(reply) if want(&reply) => return Ok(reply),
                Incoming::Reply(_) => {}
                Incoming::Disconnected => bail!("the server closed the connection"),
            }
        }
        bail!("no reply")
    };
    match tokio::time::timeout(Duration::from_secs(15), wait).await {
        Ok(result) => result,
        Err(_) => bail!("timed out waiting for the server"),
    }
}
