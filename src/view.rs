//! Remote view: while one machine drives another, the driven ("watched")
//! machine encodes the output under its cursor and streams it to the driver
//! ("viewer"), which shows it full screen. The stream rides its own Noise
//! link, so a busy video stream never delays input on the main link.
//! Moving the cursor to another output restarts the capture there; the
//! stream is raw Annex-B, so the player just carries on with the new one.
//!
//! Latency: a raw Annex-B stream has no frame lengths, so the player's parser
//! only knows a frame is complete when the next one starts: one whole frame
//! of delay (17 ms at 60 fps). gpu-screen-recorder writes each frame in
//! 32 KiB chunks and flushes the rest at the frame's end, so its stdout is a
//! SOCK_SEQPACKET socket (which keeps those write boundaries) and every short
//! write gets an access unit delimiter appended: the parser sees "next frame
//! starts here" right away and hands the frame on without waiting.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::watch;

use crate::proto::{self, MAX_PAYLOAD, Msg, PROTOCOL_VERSION, Reader, Writer};

/// A spawned task that stops when this handle is dropped.
pub struct Task(tokio::task::AbortHandle);

impl Task {
    pub fn spawn<F>(f: F) -> Task
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Task(tokio::spawn(f).abort_handle())
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn session_id() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b) | 1
}

/// Open a video link for `session` to `want` at `addr`.
pub async fn dial(
    addr: &str,
    psk: [u8; 32],
    me: &str,
    want: &str,
    session: u64,
) -> Result<(Reader, Writer)> {
    let go = async {
        let s = TcpStream::connect(addr).await?;
        let (mut r, mut w) = proto::handshake(s, &psk, true).await?;
        w.send(&Msg::Hello {
            name: me.to_string(),
            version: PROTOCOL_VERSION,
            video: Some(session),
        })
        .await?;
        let Msg::Hello { name, version, .. } = r.recv().await? else {
            bail!("expected hello")
        };
        if version != PROTOCOL_VERSION {
            bail!("{name} speaks protocol {version}, this is {PROTOCOL_VERSION}");
        }
        if name != want {
            bail!("expected {want:?}, got {name:?}");
        }
        Ok((r, w))
    };
    tokio::time::timeout(Duration::from_secs(5), go)
        .await
        .context("timed out")?
}

fn spawn(
    cmd: &[String],
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
) -> Result<tokio::process::Child> {
    let (prog, args) = cmd.split_first().context("empty command")?;
    Command::new(prog)
        .args(args)
        .envs(crate::hypr::session_env())
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        // SIGKILL: a recorder asked to stop nicely may block finishing a
        // stream nobody reads any more.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow::anyhow!("{prog} isn't installed (not found in PATH)")
            }
            _ => anyhow::Error::new(e).context(format!("can't start {prog}")),
        })
}

/// gpu-screen-recorder's write size inside a frame (ffmpeg's AVIO buffer);
/// any other size ends a frame.
const GSR_CHUNK: usize = 32768;

/// Access unit delimiter for `codec`: tells the parser a new frame begins.
fn access_unit_delimiter(codec: &str) -> Option<&'static [u8]> {
    match codec {
        "h264" => Some(&[0, 0, 0, 1, 0x09, 0xf0]),
        "hevc" => Some(&[0, 0, 0, 1, 0x46, 0x01, 0x50]),
        _ => None,
    }
}

/// The delimiter to append after each finished frame, when the capture is
/// gpu-screen-recorder (whose write pattern marks frame ends).
fn frame_end_marker(cmd: &[String]) -> Option<&'static [u8]> {
    let prog = cmd.first()?;
    if !prog.ends_with("gpu-screen-recorder") {
        return None;
    }
    let codec = cmd.iter().skip_while(|a| *a != "-k").nth(1)?;
    access_unit_delimiter(codec)
}

/// One write from the recorder → link-sized pieces, with `frame_end`
/// appended when the write finished a frame.
fn pieces(msg: &[u8], frame_end: Option<&[u8]>, max: usize) -> Vec<Vec<u8>> {
    let mut data = msg.to_vec();
    if let Some(aud) = frame_end
        && msg.len() != GSR_CHUNK
    {
        data.extend_from_slice(aud);
    }
    data.chunks(max).map(|c| c.to_vec()).collect()
}

/// A SOCK_SEQPACKET pair: (our nonblocking read end, the child's stdout).
fn packet_pair() -> Result<(tokio::io::unix::AsyncFd<OwnedFd>, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: socketpair fills `fds` with two new descriptors on success.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("socketpair");
    }
    // SAFETY: both fds were just created and are owned by nobody else.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: plain fcntl on a descriptor we own.
    unsafe {
        let fl = libc::fcntl(ours.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(ours.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    // SAFETY: `ours` stays open and unreplaced for as long as the AsyncFd owns it.
    let ours = unsafe { tokio::io::unix::AsyncFd::register(ours)? };
    Ok((ours, theirs))
}

/// Next whole write from the recorder (empty = it closed its end).
async fn recv_packet(fd: &tokio::io::unix::AsyncFd<OwnedFd>, buf: &mut [u8]) -> Result<usize> {
    loop {
        let mut guard = fd.readable().await?;
        let res = guard.try_io(|inner| {
            // SAFETY: recv into our own buffer; MSG_TRUNC reports the real size.
            let n = unsafe {
                libc::recv(
                    inner.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_TRUNC,
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match res {
            Ok(Ok(n)) if n > buf.len() => bail!("recorder wrote {n} bytes at once"),
            Ok(r) => return Ok(r?),
            Err(_would_block) => continue,
        }
    }
}

/// Watched side: capture the output named in `monitor` (restarting when it
/// changes) and send it down the link until the viewer hangs up.
pub async fn serve(
    mut w: Writer,
    mut r: Reader,
    capture: Vec<String>,
    mut monitor: watch::Receiver<String>,
) -> Result<()> {
    // The viewer never sends anything here: a finished read means it's gone.
    let (gone_tx, mut gone) = tokio::sync::oneshot::channel::<()>();
    let _reader = Task::spawn(async move {
        while r.recv().await.is_ok() {}
        let _ = gone_tx.send(());
    });
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let mon = monitor.borrow_and_update().clone();
        // A socket can't be reopened through /dev/stdout; ffmpeg's pipe:1 writes fd 1.
        let cmd: Vec<String> = crate::config::fill(&capture, &[("monitor", &mon)])
            .into_iter()
            .map(|a| {
                if a == "/dev/stdout" {
                    "pipe:1".into()
                } else {
                    a
                }
            })
            .collect();
        let frame_end = frame_end_marker(&cmd);
        log::info!("view: streaming {mon}");
        let (out, child_out) = packet_pair()?;
        let mut child = spawn(&cmd, Stdio::null(), Stdio::from(child_out), Stdio::piped())?;
        // Keep the recorder's last words for the error message.
        let last_err = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let last = last_err.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if l.contains("error") || l.contains("Error") {
                        *last.lock().unwrap() = l;
                    }
                }
            });
        }
        loop {
            tokio::select! {
                n = recv_packet(&out, &mut buf) => {
                    let n = n?;
                    if n == 0 {
                        let st = child.wait().await?;
                        let why = last_err.lock().unwrap().clone();
                        bail!("capturing {mon} stopped ({st}){}{why}", if why.is_empty() { "" } else { ": " });
                    }
                    for piece in pieces(&buf[..n], frame_end, MAX_PAYLOAD - 64) {
                        w.send(&Msg::ViewData(piece)).await?;
                    }
                }
                ch = monitor.changed() => {
                    if ch.is_err() {
                        return Ok(());
                    }
                    break;
                }
                _ = &mut gone => return Ok(()),
            }
        }
        // `child` drops here: the old capture is killed before the next starts.
    }
}

/// Viewer side: feed the stream into the player until either end stops.
pub async fn watch(mut r: Reader, w: Writer, player: Vec<String>) -> Result<()> {
    let _keep_open = w;
    let mut child = spawn(&player, Stdio::piped(), Stdio::null(), Stdio::inherit())?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    loop {
        tokio::select! {
            m = r.recv() => {
                if let Msg::ViewData(d) = m.context("the other side hung up the stream")? {
                    stdin.write_all(&d).await.context("the player closed")?;
                }
            }
            st = child.wait() => bail!("the player exited ({})", st?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn sv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Capture → link → player with stand-in commands, including a switch to
    /// another monitor mid-stream.
    #[tokio::test]
    async fn stream_follows_monitor() {
        let psk = [9u8; 32];
        let out = std::env::temp_dir().join(format!("edgehop-view-{}", std::process::id()));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (mon_tx, mon_rx) = watch::channel("DP-1".to_string());
        let server = tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let (r, w) = proto::handshake(s, &psk, false).await.unwrap();
            let capture = sv(&["sh", "-c", "printf '<{monitor}>'; exec sleep 30"]);
            serve(w, r, capture, mon_rx).await
        });
        let s = TcpStream::connect(addr).await.unwrap();
        let (r, w) = proto::handshake(s, &psk, true).await.unwrap();
        let player = vec!["sh".into(), "-c".into(), format!("cat > {}", out.display())];
        let viewer = tokio::spawn(watch(r, w, player));
        let read = || std::fs::read_to_string(&out).unwrap_or_default();
        for _ in 0..100 {
            if read().contains("<DP-1>") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        mon_tx.send_replace("HDMI-A-1".into());
        for _ in 0..100 {
            if read().contains("<HDMI-A-1>") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(read(), "<DP-1><HDMI-A-1>");
        viewer.abort();
        // The viewer hanging up ends the serving side cleanly.
        let res = tokio::time::timeout(Duration::from_secs(3), server).await;
        assert!(matches!(res, Ok(Ok(Ok(())))), "{res:?}");
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn frame_ends_get_a_delimiter() {
        let aud = access_unit_delimiter("h264");
        // A 32 KiB chunk is mid-frame: untouched.
        let mid = vec![7u8; GSR_CHUNK];
        assert_eq!(pieces(&mid, aud, 60000), vec![mid.clone()]);
        // A short write ends the frame: delimiter appended.
        assert_eq!(
            pieces(&[1, 2, 3], aud, 60000),
            vec![vec![1, 2, 3, 0, 0, 0, 1, 0x09, 0xf0]]
        );
        // Big writes are split to fit the link.
        let big = pieces(&[5u8; 100], None, 40);
        assert_eq!(
            big.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![40, 40, 20]
        );
        let gsr = sv(&[
            "gpu-screen-recorder",
            "-w",
            "DP-1",
            "-k",
            "hevc",
            "-o",
            "pipe:1",
        ]);
        assert_eq!(frame_end_marker(&gsr), access_unit_delimiter("hevc"));
        assert_eq!(frame_end_marker(&sv(&["sh", "-c", "x"])), None);
    }

    #[tokio::test]
    async fn missing_recorder_says_not_installed() {
        let cmd = sv(&["edgehop-no-such-recorder", "-w", "DP-1"]);
        let err = spawn(&cmd, Stdio::null(), Stdio::null(), Stdio::null()).unwrap_err();
        assert!(err.to_string().contains("isn't installed"), "{err}");
    }

    #[tokio::test]
    async fn failing_capture_reports_why() {
        let psk = [9u8; 32];
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (_tx, mon_rx) = watch::channel("DP-1".to_string());
        let server = tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let (r, w) = proto::handshake(s, &psk, false).await.unwrap();
            let capture = sv(&["sh", "-c", "echo 'gsr error: no such monitor' >&2; exit 3"]);
            serve(w, r, capture, mon_rx).await
        });
        let s = TcpStream::connect(addr).await.unwrap();
        let _link = proto::handshake(s, &psk, true).await.unwrap();
        let err = server.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("no such monitor"), "{err}");
    }
}
