//! Remote view quality presets, for slow links (a phone hotspot, a long
//! Tailscale relay). The machine being watched runs the encoder, so it applies
//! the preset: the viewer's request is capped, and `gpu-screen-recorder` scales
//! the picture down. The choice is kept in `$XDG_STATE_HOME/edgehop/view-quality`.

use std::path::PathBuf;

pub struct Quality {
    pub name: &'static str,
    /// Upper bounds on what the viewer asked for.
    pub max_kbps: u32,
    pub max_fps: u32,
    /// Largest output size (aspect ratio kept); None = native.
    pub size: Option<(u32, u32)>,
    /// Send sound (raw PCM costs ~1.5 Mb/s).
    pub audio: bool,
    pub label: &'static str,
}

pub const QUALITIES: &[Quality] = &[
    Quality {
        name: "full",
        max_kbps: u32::MAX,
        max_fps: u32::MAX,
        size: None,
        audio: true,
        label: "Full: native, as asked (20 Mb/s), sound",
    },
    Quality {
        name: "medium",
        max_kbps: 8000,
        max_fps: 60,
        size: Some((1920, 1080)),
        audio: true,
        label: "Medium: 1080p, 60 fps, 8 Mb/s, sound",
    },
    Quality {
        name: "low",
        max_kbps: 3000,
        max_fps: 30,
        size: Some((1280, 720)),
        audio: false,
        label: "Low: 720p, 30 fps, 3 Mb/s, no sound",
    },
    Quality {
        name: "potato",
        max_kbps: 1000,
        max_fps: 24,
        size: Some((960, 540)),
        audio: false,
        label: "Potato: 540p, 24 fps, 1 Mb/s, no sound",
    },
];

pub fn find(name: &str) -> Option<&'static Quality> {
    QUALITIES.iter().find(|q| q.name.eq_ignore_ascii_case(name))
}

pub fn names() -> String {
    QUALITIES
        .iter()
        .map(|q| q.name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("edgehop/view-quality")
}

/// The saved preset (Full when none is saved).
pub fn load() -> &'static Quality {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| find(s.trim()))
        .unwrap_or(&QUALITIES[0])
}

pub fn save(q: &Quality) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, format!("{}\n", q.name)) {
        log::warn!("can't save view quality to {}: {e}", path.display());
    }
}

impl Quality {
    pub fn fps(&self, asked: u32) -> u32 {
        asked.min(self.max_fps).max(1)
    }

    pub fn kbps(&self, asked: u32) -> u32 {
        asked.min(self.max_kbps).max(100)
    }

    /// Add the size limit to a gpu-screen-recorder command (other capture
    /// commands are left alone: they get only the capped fps/bitrate).
    pub fn scale(&self, cmd: Vec<String>) -> Vec<String> {
        let Some((w, h)) = self.size else {
            return cmd;
        };
        let is_gsr = cmd
            .first()
            .is_some_and(|p| p.ends_with("gpu-screen-recorder"));
        if !is_gsr || cmd.iter().any(|a| a == "-s") {
            return cmd;
        }
        let mut out = Vec::with_capacity(cmd.len() + 2);
        out.push(cmd[0].clone());
        out.push("-s".into());
        out.push(format!("{w}x{h}"));
        out.extend(cmd.into_iter().skip(1));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn caps_and_scales() {
        let low = find("LOW").unwrap();
        assert_eq!(low.fps(120), 30);
        assert_eq!(low.fps(20), 20);
        assert_eq!(low.kbps(20000), 3000);
        assert_eq!(
            low.scale(v(&["/x/gpu-screen-recorder", "-w", "DP-3"])),
            v(&["/x/gpu-screen-recorder", "-s", "1280x720", "-w", "DP-3"])
        );
        // Other recorders and commands that already pick a size stay as they are.
        assert_eq!(low.scale(v(&["wf-recorder"])), v(&["wf-recorder"]));
        let sized = v(&["gpu-screen-recorder", "-s", "640x360"]);
        assert_eq!(low.scale(sized.clone()), sized);
        let full = find("full").unwrap();
        assert_eq!(full.kbps(20000), 20000);
        assert_eq!(full.fps(120), 120);
        assert_eq!(
            full.scale(v(&["gpu-screen-recorder"])),
            v(&["gpu-screen-recorder"])
        );
        assert!(find("ultra").is_none());
    }
}
