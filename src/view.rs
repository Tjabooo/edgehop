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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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

/// Open a video (or, with `audio`, sound) link for `session` to `want` at `addr`.
pub async fn dial(
    addr: &str,
    psk: [u8; 32],
    me: &str,
    want: &str,
    session: u64,
    audio: bool,
) -> Result<(Reader, Writer)> {
    let go = async {
        let s = TcpStream::connect(addr).await?;
        let (mut r, mut w) = proto::handshake(s, &psk, true).await?;
        w.send(&Msg::Hello {
            name: me.to_string(),
            version: PROTOCOL_VERSION,
            video: Some(session),
            audio,
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
        // gpu-screen-recorder builds that read it hand out each frame as soon as
        // it is encoded instead of holding ~2 (VAAPI async_depth); others ignore it.
        .env("GSR_ASYNC_DEPTH", "1")
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

// ------------------------------------------------------------------ sound
//
// Sound rides a third link as raw PCM (s16 stereo, 48 kHz: ~1.5 Mb/s, small
// next to the video): no codec delay, and pw-record/pw-play do the work.
// Delay stays bounded on the viewer: whatever the player hasn't read yet is
// measured before each write, and a chunk that would push it past
// AUDIO_MAX_QUEUED is dropped (a tiny skip instead of growing delay after
// a network hiccup or clock drift).

/// Bytes per frame on the wire (s16 stereo): chunks are whole frames.
const AUDIO_FRAME: usize = 4;
/// Largest chunk: 10 ms, well under PIPE_BUF so a write is all or nothing.
const AUDIO_CHUNK: usize = 1920;
/// Most sound waiting for the player: 40 ms.
const AUDIO_MAX_QUEUED: usize = 7680;

/// Kill-on-drop child plus the last error line it printed.
fn spawn_logged(
    cmd: &[String],
    stdin: Stdio,
    stdout: Stdio,
) -> Result<(tokio::process::Child, Arc<Mutex<String>>)> {
    let mut child = spawn(cmd, stdin, stdout, Stdio::piped())?;
    let last_err = Arc::new(Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let last = last_err.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                *last.lock().unwrap() = l;
            }
        });
    }
    Ok((child, last_err))
}

/// Watched side: record `capture`'s raw PCM and send it until the viewer hangs up.
pub async fn serve_audio(mut w: Writer, mut r: Reader, capture: Vec<String>) -> Result<()> {
    let (gone_tx, mut gone) = tokio::sync::oneshot::channel::<()>();
    let _reader = Task::spawn(async move {
        while r.recv().await.is_ok() {}
        let _ = gone_tx.send(());
    });
    let (mut child, last_err) = spawn_logged(&capture, Stdio::null(), Stdio::piped())?;
    let mut out = child.stdout.take().context("no stdout")?;
    let mut buf = vec![0u8; 16384];
    let mut have = 0;
    loop {
        tokio::select! {
            n = out.read(&mut buf[have..]) => {
                let n = n?;
                if n == 0 {
                    let st = child.wait().await?;
                    let why = last_err.lock().unwrap().clone();
                    bail!("sound capture stopped ({st}){}{why}", if why.is_empty() { "" } else { ": " });
                }
                have += n;
                let whole = have - have % AUDIO_FRAME;
                for c in buf[..whole].chunks(AUDIO_CHUNK) {
                    w.send(&Msg::ViewData(c.to_vec())).await?;
                }
                buf.copy_within(whole..have, 0);
                have -= whole;
            }
            _ = &mut gone => return Ok(()),
        }
    }
}

/// Bytes sitting in a pipe (FIONREAD works on either end).
fn pipe_queued(fd: i32) -> usize {
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int.
    let r = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) };
    if r < 0 { 0 } else { n.max(0) as usize }
}

/// What to do with one chunk, given how much the player hasn't read yet.
fn audio_fits(queued: usize, chunk: usize) -> bool {
    queued + chunk <= AUDIO_MAX_QUEUED
}

/// Viewer side: play the sound with `player` (raw PCM on stdin).
pub async fn play_audio(mut r: Reader, w: Writer, player: Vec<String>) -> Result<()> {
    let _keep_open = w;
    let (mut child, last_err) = spawn_logged(&player, Stdio::piped(), Stdio::null())?;
    let stdin = child.stdin.take().context("no stdin")?;
    let fd = stdin.as_raw_fd();
    let mut dropped = 0u64;
    loop {
        tokio::select! {
            m = r.recv() => {
                let Msg::ViewData(d) = m.context("the other side hung up the sound")? else {
                    continue;
                };
                if d.len() % AUDIO_FRAME != 0 || d.len() > AUDIO_CHUNK {
                    continue;
                }
                if !audio_fits(pipe_queued(fd), d.len()) {
                    dropped += 1;
                    if dropped.is_power_of_two() {
                        log::debug!("view: sound behind, dropped {dropped} chunks so far");
                    }
                    continue;
                }
                // tokio set the pipe nonblocking; ≤ PIPE_BUF bytes go in whole or not at all.
                // SAFETY: write from our buffer to the player's stdin pipe.
                let n = unsafe { libc::write(fd, d.as_ptr().cast(), d.len()) };
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        return Err(e).context("the sound player closed");
                    }
                    dropped += 1;
                }
            }
            st = child.wait() => {
                let why = last_err.lock().unwrap().clone();
                bail!("the sound player exited ({}){}{why}", st?, if why.is_empty() { "" } else { ": " })
            }
        }
    }
}

/// Drop `--flag ""` pairs left by an empty placeholder (e.g. no audio_source:
/// pw-record then records the default output).
pub fn drop_empty_options(cmd: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(cmd.len());
    let mut it = cmd.into_iter().peekable();
    while let Some(a) = it.next() {
        if a.starts_with('-') && a.len() > 1 && it.peek().is_some_and(|n| n.is_empty()) {
            it.next();
            continue;
        }
        out.push(a);
    }
    out
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

    #[test]
    fn audio_queue_bound_and_empty_options() {
        assert!(audio_fits(0, AUDIO_CHUNK));
        assert!(audio_fits(AUDIO_MAX_QUEUED - AUDIO_CHUNK, AUDIO_CHUNK));
        assert!(!audio_fits(AUDIO_MAX_QUEUED - AUDIO_CHUNK + 4, AUDIO_CHUNK));
        assert_eq!(
            drop_empty_options(sv(&["pw-record", "--target", "", "-"])),
            sv(&["pw-record", "-"])
        );
        assert_eq!(
            drop_empty_options(sv(&["pw-record", "--target", "ember_game", "-"])),
            sv(&["pw-record", "--target", "ember_game", "-"])
        );
    }

    /// Sound end to end with stand-ins: whole frames arrive, odd bytes wait.
    #[tokio::test]
    async fn sound_reaches_the_player() {
        let psk = [3u8; 32];
        let out = std::env::temp_dir().join(format!("edgehop-sound-{}", std::process::id()));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let (r, w) = proto::handshake(s, &psk, false).await.unwrap();
            // 4002 bytes: 1000 whole frames + 2 stray bytes that never go out.
            serve_audio(
                w,
                r,
                sv(&[
                    "sh",
                    "-c",
                    "head -c 4002 /dev/zero | tr '\\0' 'a'; exec sleep 30",
                ]),
            )
            .await
        });
        let s = TcpStream::connect(addr).await.unwrap();
        let (r, w) = proto::handshake(s, &psk, true).await.unwrap();
        let player = vec!["sh".into(), "-c".into(), format!("cat > {}", out.display())];
        let viewer = tokio::spawn(play_audio(r, w, player));
        let len = || std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        for _ in 0..100 {
            if len() >= 4000 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(len(), 4000);
        viewer.abort();
        let res = tokio::time::timeout(Duration::from_secs(3), server).await;
        assert!(matches!(res, Ok(Ok(Ok(())))), "{res:?}");
        let _ = std::fs::remove_file(&out);
    }
}
