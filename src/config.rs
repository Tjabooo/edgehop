use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Deserialize;

use crate::geometry::Side;

pub const DEFAULT_PORT: u16 = 24801;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// This machine's name. The peer must list it under the same name.
    pub name: String,
    /// Address to accept peers on, e.g. "0.0.0.0:24801". Leave it out if
    /// this machine only dials out.
    #[serde(default)]
    pub listen: Option<String>,
    /// Shared secret (32 bytes, base64). Same value on every machine.
    pub psk: String,
    /// Hold these together while controlling another machine to snap back.
    #[serde(default = "default_release_keys")]
    pub release_keys: Vec<String>,
    /// Don't cross over while the focused window is fullscreen (games).
    #[serde(default = "yes")]
    pub block_when_fullscreen: bool,
    /// Show desktop notifications on crossing (which machine has your input,
    /// and how to get it back).
    #[serde(default = "yes")]
    pub notify: bool,
    /// Your `edgehop switch` keybind, named in the notifications, e.g. "Super+Alt+K".
    #[serde(default)]
    pub switch_hint: Option<String>,
    /// Device names (substring, case-insensitive) that are never captured.
    #[serde(default)]
    pub ignore_devices: Vec<String>,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    /// Remote view: watch the screen of the machine you're driving.
    #[serde(default)]
    pub view: ViewConfig,
}

/// `[view]`: see a peer's screen here while you drive it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ViewConfig {
    /// Start with remote view switched on (else `edgehop view` turns it on).
    pub on: bool,
    /// What you ask the other machine for when watching it.
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// "h264" or "hevc".
    pub codec: String,
    /// Your `edgehop view` keybind, named in the notifications.
    pub hint: Option<String>,
    /// Command printing a raw Annex-B stream of one output to stdout.
    /// Placeholders: {monitor} {fps} {kbps} {codec}.
    pub capture: Vec<String>,
    /// Command showing that stream (read from stdin) full screen.
    /// Placeholders: {codec} {peer}.
    pub player: Vec<String>,
}

impl Default for ViewConfig {
    fn default() -> ViewConfig {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        ViewConfig {
            on: false,
            fps: 60,
            bitrate_kbps: 20000,
            codec: "h264".into(),
            hint: None,
            capture: v(&[
                "gpu-screen-recorder",
                "-w",
                "{monitor}",
                "-f",
                "{fps}",
                "-k",
                "{codec}",
                "-bm",
                "cbr",
                "-q",
                "{kbps}",
                "-tune",
                "performance",
                "-c",
                "{codec}",
                "-o",
                "/dev/stdout",
            ]),
            player: v(&[
                "mpv",
                "--no-config",
                "--really-quiet",
                "--profile=low-latency",
                "--untimed",
                "--cache=no",
                "--demuxer-lavf-format={codec}",
                "--demuxer-lavf-probesize=32",
                "--demuxer-lavf-analyzeduration=0",
                "--hwdec=auto",
                "--no-audio",
                "--fs",
                "--no-border",
                "--wayland-app-id=edgehop-view",
                "--title=edgehop: {peer}",
                "--cursor-autohide=always",
                "--no-osc",
                "--osd-level=0",
                "--no-input-default-bindings",
                "--input-vo-keyboard=no",
                "--background-color=#000000",
                "-",
            ]),
        }
    }
}

/// Fill `{name}` placeholders in a command template.
pub fn fill(template: &[String], vars: &[(&str, &str)]) -> Vec<String> {
    template
        .iter()
        .map(|arg| {
            let mut a = arg.clone();
            for (k, v) in vars {
                a = a.replace(&format!("{{{k}}}"), v);
            }
            a
        })
        .collect()
}

pub fn valid_codec(c: &str) -> bool {
    matches!(c, "h264" | "hevc")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    pub name: String,
    /// Where the peer's screens are, seen from this machine.
    pub side: Side,
    /// host:port to dial. Leave it out if the peer dials this machine.
    #[serde(default)]
    pub address: Option<String>,
    /// Only these outputs' edges lead to the peer (e.g. ["DP-3"]); empty = all.
    #[serde(default)]
    pub monitors: Vec<String>,
    /// Keep pushing against the edge this long before crossing (ms), so a
    /// quick flick to a bar at that edge doesn't throw you over.
    #[serde(default)]
    pub edge_delay_ms: u64,
}

fn yes() -> bool {
    true
}

fn default_release_keys() -> Vec<String> {
    vec![
        "KEY_LEFTCTRL".into(),
        "KEY_LEFTALT".into(),
        "KEY_ESC".into(),
    ]
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.psk_bytes()?;
        cfg.release_codes()?;
        if !valid_codec(&cfg.view.codec) {
            bail!(
                "view.codec must be \"h264\" or \"hevc\", not {:?}",
                cfg.view.codec
            );
        }
        if cfg.view.capture.is_empty() || cfg.view.player.is_empty() {
            bail!("view.capture and view.player can't be empty");
        }
        let mut sides = Vec::new();
        for p in &cfg.peers {
            if p.name == cfg.name {
                bail!("peer {:?} has this machine's own name", p.name);
            }
            if sides.contains(&p.side) {
                bail!("two peers on the {:?} side", p.side);
            }
            sides.push(p.side);
        }
        Ok(cfg)
    }

    pub fn psk_bytes(&self) -> Result<[u8; 32]> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(self.psk.trim())
            .context("psk is not valid base64 (make one with `edgehop keygen`)")?;
        raw.try_into()
            .map_err(|_| anyhow::anyhow!("psk must be 32 bytes (make one with `edgehop keygen`)"))
    }

    pub fn release_codes(&self) -> Result<Vec<u16>> {
        self.release_keys
            .iter()
            .map(|k| {
                k.parse::<evdev::KeyCode>()
                    .map(|c| c.code())
                    .map_err(|_| anyhow::anyhow!("unknown key {k:?} in release_keys"))
            })
            .collect()
    }
}

/// "host" → "host:24801"; "host:port" and "[v6]:port" stay as they are.
pub fn with_port(addr: &str) -> String {
    let has_port = match addr.rsplit_once(':') {
        Some((host, port)) => {
            port.parse::<u16>().is_ok() && (!host.contains(':') || host.ends_with(']'))
        }
        None => false,
    };
    if has_port {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_PORT}")
    }
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("edgehop").join("config.toml")
}

pub fn new_psk() -> Result<String> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(with_port("10.0.0.2"), "10.0.0.2:24801");
        assert_eq!(with_port("box:9000"), "box:9000");
        assert_eq!(with_port("[::1]:9000"), "[::1]:9000");
        assert_eq!(with_port("0.0.0.0"), "0.0.0.0:24801");
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.peers.len(), 1);
        assert!(cfg.release_codes().is_ok());
        assert!(cfg.view.on);
        assert_eq!(cfg.view.capture[0], "gpu-screen-recorder");
    }

    #[test]
    fn view_defaults_and_fill() {
        let cfg: Config = toml::from_str("name = \"a\"\npsk = \"x\"\n").unwrap();
        assert!(!cfg.view.on);
        let cmd = fill(
            &cfg.view.capture,
            &[
                ("monitor", "DP-3"),
                ("fps", "60"),
                ("kbps", "9000"),
                ("codec", "hevc"),
            ],
        );
        assert!(cmd.windows(2).any(|w| w == ["-w", "DP-3"]));
        assert!(cmd.windows(2).any(|w| w == ["-q", "9000"]));
        assert!(cmd.windows(2).any(|w| w == ["-c", "hevc"]));
        let play = fill(&cfg.view.player, &[("codec", "hevc"), ("peer", "magi")]);
        assert!(play.contains(&"--demuxer-lavf-format=hevc".to_string()));
        assert!(play.contains(&"--title=edgehop: magi".to_string()));
    }
}
