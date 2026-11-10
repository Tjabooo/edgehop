//! Desktop notifications (notify-send) so it's always obvious which machine
//! has your keyboard and how to get it back.

use std::sync::{Arc, Mutex};

use crate::geometry::Side;

#[derive(Clone)]
pub struct Notifier {
    enabled: bool,
    /// Id of the persistent "Driving …" notification, to replace it on return.
    driving_id: Arc<Mutex<Option<u32>>>,
}

pub fn edge_word(side: Side) -> &'static str {
    match side {
        Side::Left => "left edge",
        Side::Right => "right edge",
        Side::Top => "top edge",
        Side::Bottom => "bottom edge",
    }
}

/// ["KEY_LEFTCTRL", "KEY_LEFTALT", "KEY_ESC"] → "Left Ctrl+Left Alt+Esc"
pub fn key_names(keys: &[String]) -> String {
    keys.iter()
        .map(|k| {
            let k = k.trim_start_matches("KEY_");
            let (prefix, rest) = if let Some(r) = k.strip_prefix("LEFT") {
                ("Left ", r)
            } else if let Some(r) = k.strip_prefix("RIGHT") {
                ("Right ", r)
            } else {
                ("", k)
            };
            let rest = match rest {
                "CTRL" => "Ctrl".to_string(),
                "ALT" => "Alt".to_string(),
                "META" => "Super".to_string(),
                "SHIFT" => "Shift".to_string(),
                "ESC" => "Esc".to_string(),
                other => {
                    let mut c = other.chars();
                    match c.next() {
                        Some(f) => f.to_string() + &c.as_str().to_lowercase(),
                        None => String::new(),
                    }
                }
            };
            format!("{prefix}{rest}")
        })
        .collect::<Vec<_>>()
        .join("+")
}

impl Notifier {
    pub fn new(enabled: bool) -> Notifier {
        Notifier {
            enabled,
            driving_id: Arc::new(Mutex::new(None)),
        }
    }

    fn spawn(&self, args: Vec<String>, store_id: bool) {
        if !self.enabled {
            return;
        }
        let slot = self.driving_id.clone();
        tokio::spawn(async move {
            let out = tokio::process::Command::new("notify-send")
                .args(&args)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .await;
            match out {
                Ok(o) if store_id => {
                    let id = String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .parse::<u32>()
                        .ok();
                    *slot.lock().unwrap() = id;
                }
                Ok(_) => {}
                Err(e) => log::debug!("notify-send: {e}"),
            }
        });
    }
}

/// "hold Left Ctrl+Left Alt+Esc" (+ ", or press <bind>" when a switch bind is configured).
pub fn ways_back(release_keys: &[String], switch_hint: Option<&str>) -> String {
    let mut s = String::new();
    if !release_keys.is_empty() {
        s += &format!("hold {}", key_names(release_keys));
    }
    if let Some(h) = switch_hint.filter(|h| !h.is_empty()) {
        if !s.is_empty() {
            s += ", or ";
        }
        s += &format!("press {h}");
    }
    s
}

impl Notifier {
    /// Controller side: shown (and kept) while this machine's input drives `peer`.
    pub fn driving(&self, peer: &str, peer_edge: Side, ways: &str) {
        let body = format!(
            "Your keyboard and mouse are on {peer}.\nCome back: push the cursor off {peer}'s {}, {ways}.",
            edge_word(peer_edge)
        );
        let args = vec![
            "-a".into(),
            "edgehop".into(),
            "-p".into(),
            "-t".into(),
            "0".into(),
            "-h".into(),
            "string:x-dunst-stack-tag:edgehop".into(),
            format!("Driving {peer}"),
            body,
        ];
        self.spawn(args, true);
    }

    /// Controller side: input is home again.
    pub fn back(&self, me: &str) {
        let id = self.driving_id.lock().unwrap().take();
        let mut args = vec![
            "-a".into(),
            "edgehop".into(),
            "-t".into(),
            "1500".into(),
            "-h".into(),
            "string:x-dunst-stack-tag:edgehop".into(),
        ];
        if let Some(id) = id {
            args.push("-r".into());
            args.push(id.to_string());
        }
        args.push(format!("Back on {me}"));
        self.spawn(args, false);
    }

    /// Receiver side: someone else's keyboard just arrived here.
    pub fn arrived(&self, from: &str, me: &str, back_edge: Side, ways: &str) {
        let body = format!(
            "Back to {from}: push the cursor off this {}, {ways}.",
            edge_word(back_edge)
        );
        let args = vec![
            "-a".into(),
            "edgehop".into(),
            "-t".into(),
            "4000".into(),
            "-h".into(),
            "string:x-dunst-stack-tag:edgehop".into(),
            format!("{from} is driving {me}"),
            body,
        ];
        self.spawn(args, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_keys() {
        let k = ["KEY_LEFTCTRL", "KEY_LEFTALT", "KEY_ESC"].map(String::from);
        assert_eq!(key_names(&k), "Left Ctrl+Left Alt+Esc");
        assert_eq!(
            key_names(&["KEY_RIGHTMETA".into(), "KEY_F12".into()]),
            "Right Super+F12"
        );
        assert_eq!(
            ways_back(&k, Some("Super+Alt+K")),
            "hold Left Ctrl+Left Alt+Esc, or press Super+Alt+K"
        );
        assert_eq!(ways_back(&k, None), "hold Left Ctrl+Left Alt+Esc");
    }
}
