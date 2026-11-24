//! Minimal Hyprland IPC client: cursor position, monitor layout, fullscreen
//! state, cursor warping.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::geometry::{Layout, Rect};

const IPC_TIMEOUT: Duration = Duration::from_millis(500);
const LAYOUT_TTL: Duration = Duration::from_secs(2);
const FULLSCREEN_TTL: Duration = Duration::from_millis(750);

pub struct Hypr {
    /// Highest refresh rate among enabled monitors at the last layout query.
    max_hz: Option<f64>,
    socket: Option<PathBuf>,
    layout: Option<(Instant, Layout)>,
    fullscreen: Option<(Instant, bool)>,
    /// Hyprland ≥0.55 with Lua config takes `hl.dsp.*` dispatchers; older
    /// versions take `movecursor x y`. Learned on first warp.
    legacy_dispatch: Option<bool>,
}

#[derive(Deserialize)]
struct CursorPos {
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
struct Monitor {
    #[serde(default)]
    name: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    #[serde(default, rename = "refreshRate")]
    refresh_rate: f64,
    #[serde(default)]
    transform: i64,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
struct ActiveWindow {
    #[serde(default)]
    fullscreen: serde_json::Value,
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

/// Find the running Hyprland's command socket. Prefers
/// $HYPRLAND_INSTANCE_SIGNATURE, else the newest instance (the daemon may
/// run from a systemd user unit without the variable, and Hyprland may restart).
fn find_socket() -> Option<PathBuf> {
    let base = runtime_dir().join("hypr");
    if let Some(sig) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        let p = base.join(sig).join(".socket.sock");
        if p.exists() {
            return Some(p);
        }
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&base).ok()?.flatten() {
        let p = entry.path().join(".socket.sock");
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        let t = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
            best = Some((t, p));
        }
    }
    best.map(|(_, p)| p)
}

/// Session variables a child (screen recorder, video player) needs to reach
/// the compositor but the daemon may lack: a systemd user unit started
/// before Hyprland exported its environment has no WAYLAND_DISPLAY, and
/// gpu-screen-recorder then fails with "failed to create window". Only
/// variables missing from our own environment are returned.
pub fn session_env() -> Vec<(&'static str, std::ffi::OsString)> {
    let mut out = Vec::new();
    let has = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if !has("WAYLAND_DISPLAY")
        && let Some(name) = newest_wayland_socket(&runtime_dir())
    {
        out.push(("WAYLAND_DISPLAY", name));
    }
    if !has("HYPRLAND_INSTANCE_SIGNATURE")
        && let Some(sig) = find_socket()
            .as_deref()
            .and_then(|p| p.parent())
            .and_then(|d| d.file_name())
    {
        out.push(("HYPRLAND_INSTANCE_SIGNATURE", sig.to_os_string()));
    }
    out
}

/// The newest `wayland-N` socket in `dir` (skipping `.lock` files).
fn newest_wayland_socket(dir: &std::path::Path) -> Option<std::ffi::OsString> {
    use std::os::unix::fs::FileTypeExt;
    let mut best: Option<(std::time::SystemTime, std::ffi::OsString)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if !s.starts_with("wayland-") || s.ends_with(".lock") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.file_type().is_socket() {
            continue;
        }
        let t = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
            best = Some((t, name));
        }
    }
    best.map(|(_, n)| n)
}

impl Hypr {
    pub fn new() -> Hypr {
        Hypr {
            socket: None,
            layout: None,
            fullscreen: None,
            legacy_dispatch: None,
            max_hz: None,
        }
    }

    async fn request(&mut self, req: &str) -> Result<String> {
        for attempt in 0..2 {
            if self.socket.is_none() || attempt == 1 {
                self.socket = find_socket();
            }
            let Some(path) = self.socket.clone() else {
                bail!("Hyprland is not running (no IPC socket)");
            };
            let fut = async {
                let mut s = UnixStream::connect(&path).await?;
                s.write_all(req.as_bytes()).await?;
                let mut out = String::new();
                s.read_to_string(&mut out).await?;
                Ok::<_, std::io::Error>(out)
            };
            match tokio::time::timeout(IPC_TIMEOUT, fut).await {
                Ok(Ok(out)) => return Ok(out),
                Ok(Err(e)) if attempt == 0 => log::debug!("hyprland ipc: {e}, retrying"),
                Ok(Err(e)) => return Err(e).context("hyprland ipc"),
                Err(_) => bail!("hyprland ipc timed out"),
            }
        }
        unreachable!()
    }

    /// This machine's fastest screen refresh rate, from the last layout query
    /// (which runs on every edge check, so it's fresh when a crossing starts).
    pub fn screen_hz(&self) -> Option<f64> {
        self.max_hz
    }

    pub async fn cursor(&mut self) -> Result<(f64, f64)> {
        let out = self.request("j/cursorpos").await?;
        let c: CursorPos =
            serde_json::from_str(&out).with_context(|| format!("cursorpos: {out}"))?;
        Ok((c.x, c.y))
    }

    pub async fn layout(&mut self) -> Result<Layout> {
        if let Some((t, l)) = &self.layout
            && t.elapsed() < LAYOUT_TTL
        {
            return Ok(l.clone());
        }
        let out = self.request("j/monitors").await?;
        let mons: Vec<Monitor> =
            serde_json::from_str(&out).with_context(|| format!("monitors: {out}"))?;
        let mons: Vec<&Monitor> = mons
            .iter()
            .filter(|m| !m.disabled && m.scale > 0.0)
            .collect();
        self.max_hz = mons
            .iter()
            .map(|m| m.refresh_rate)
            .filter(|hz| *hz > 0.0)
            .reduce(f64::max);
        let names: Vec<String> = mons.iter().map(|m| m.name.clone()).collect();
        let monitors: Vec<Rect> = mons
            .iter()
            .map(|m| {
                let (mut w, mut h) = ((m.width / m.scale).round(), (m.height / m.scale).round());
                if m.transform % 2 == 1 {
                    std::mem::swap(&mut w, &mut h);
                }
                Rect {
                    x: m.x,
                    y: m.y,
                    w,
                    h,
                }
            })
            .collect();
        if monitors.is_empty() {
            return Err(anyhow!("no monitors"));
        }
        let layout = Layout { monitors, names };
        self.layout = Some((Instant::now(), layout.clone()));
        Ok(layout)
    }

    /// Is the focused window fullscreen (cached briefly)?
    pub async fn fullscreen(&mut self) -> bool {
        if let Some((t, v)) = self.fullscreen
            && t.elapsed() < FULLSCREEN_TTL
        {
            return v;
        }
        let v = match self.request("j/activewindow").await {
            Ok(out) => serde_json::from_str::<ActiveWindow>(&out)
                .map(|w| match w.fullscreen {
                    serde_json::Value::Bool(b) => b,
                    // 0 none, 1 maximized, 2 fullscreen, 3 both
                    serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) & 2 != 0,
                    _ => false,
                })
                .unwrap_or(false),
            Err(_) => false,
        };
        self.fullscreen = Some((Instant::now(), v));
        v
    }

    pub async fn warp(&mut self, x: f64, y: f64) -> Result<()> {
        let (x, y) = (x.round() as i64, y.round() as i64);
        let lua = format!("dispatch hl.dsp.cursor.move({{x={x},y={y}}})");
        let legacy = format!("dispatch movecursor {x} {y}");
        let order: [&str; 2] = if self.legacy_dispatch == Some(true) {
            [&legacy, &lua]
        } else {
            [&lua, &legacy]
        };
        let mut last = String::new();
        for (i, req) in order.iter().enumerate() {
            let out = self.request(req).await?;
            if out.trim() == "ok" {
                let used_legacy = (i == 1) != (self.legacy_dispatch == Some(true));
                self.legacy_dispatch = Some(used_legacy);
                return Ok(());
            }
            last = out;
        }
        bail!("cursor warp failed: {}", last.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_wayland_socket_skipping_locks_and_files() {
        let dir = std::env::temp_dir().join(format!("edgehop-wl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("wayland-0.lock"), "").unwrap();
        std::fs::write(dir.join("wayland-9"), "").unwrap(); // not a socket
        let _l = std::os::unix::net::UnixListener::bind(dir.join("wayland-1")).unwrap();
        assert_eq!(newest_wayland_socket(&dir).unwrap(), "wayland-1");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
