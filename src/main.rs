mod config;
mod daemon;
mod devices;
mod geometry;
mod hypr;
mod mirror;
mod notify;
mod proto;
mod quality;
mod view;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const USAGE: &str = "edgehop: software KVM for Wayland (Hyprland). Push the cursor off a screen edge to drive the machine next to it.

usage:
  edgehop run [--config FILE] [-v]   run the daemon
  edgehop check [--config FILE]      show which devices would be captured and the screen layout
  edgehop keygen                     print a fresh shared key for `psk`
  edgehop status                     what the running daemon is doing
  edgehop switch [PEER]              jump to a peer now (or back home); bind it to a key
  edgehop view [on|off]              remote view: see the screen of the machine you drive; bind it to a key
  edgehop view quality [NAME]        quality when this machine is watched: full, medium, low, potato
  edgehop view pick                  choose that quality in a rofi menu (opens by itself when you switch view on)
  edgehop toggle | enable | disable  switch edge crossing on/off
  edgehop release                    take local input back right now

config: $XDG_CONFIG_HOME/edgehop/config.toml (see config.example.toml)";

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path = config::default_path();
    let mut verbose = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => {
                let p = args.get(i + 1).context("--config needs a path")?.clone();
                config_path = PathBuf::from(p);
                args.drain(i..i + 2);
            }
            "-v" | "--verbose" => {
                verbose = true;
                args.remove(i);
            }
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("edgehop {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => i += 1,
        }
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if verbose {
        "edgehop=debug"
    } else {
        "edgehop=info"
    }))
    .format_timestamp_millis()
    .init();

    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    match cmd {
        "run" => {
            let cfg = config::Config::load(&config_path)?;
            rt.block_on(daemon::run(cfg))
        }
        "keygen" => {
            println!("{}", config::new_psk()?);
            Ok(())
        }
        "check" => rt.block_on(check(&config_path)),
        "status" | "toggle" | "enable" | "disable" | "on" | "off" | "release" => {
            rt.block_on(control(cmd))
        }
        "view" if args.get(1).map(String::as_str) == Some("pick") => rt.block_on(pick_quality()),
        "switch" | "view" => rt.block_on(control(&args.join(" "))),
        _ => {
            eprintln!("{USAGE}");
            bail!("unknown command {cmd:?}")
        }
    }
}

/// `edgehop view pick`: a rofi menu of the quality presets.
async fn pick_quality() -> Result<()> {
    let current = quality::load();
    let lines: Vec<String> = quality::QUALITIES
        .iter()
        .map(|q| {
            let mark = if q.name == current.name {
                "● "
            } else {
                "   "
            };
            format!("{mark}{}", q.label)
        })
        .collect();
    let row = quality::QUALITIES
        .iter()
        .position(|q| q.name == current.name)
        .unwrap_or(0);
    let mut child = tokio::process::Command::new("rofi")
        .args([
            "-dmenu",
            "-i",
            "-no-custom",
            "-format",
            "i",
            "-p",
            "View quality",
        ])
        .args([
            "-mesg",
            "Lower it on a slow connection. Esc keeps the current one.",
        ])
        .args(["-selected-row", &row.to_string()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("can't run rofi")?;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(lines.join("\n").as_bytes()).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    let picked = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|i| quality::QUALITIES.get(i));
    match picked {
        Some(q) => control(&format!("view quality {}", q.name)).await,
        None => Ok(()),
    }
}

async fn control(cmd: &str) -> Result<()> {
    let path = daemon::socket_path();
    let mut s = tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("edgehop is not running ({})", path.display()))?;
    s.write_all(format!("{cmd}\n").as_bytes()).await?;
    let mut out = String::new();
    s.read_to_string(&mut out).await?;
    print!("{out}");
    Ok(())
}

async fn check(config_path: &std::path::Path) -> Result<()> {
    let cfg = match config::Config::load(config_path) {
        Ok(c) => {
            println!(
                "config: {} OK ({} peer(s))",
                config_path.display(),
                c.peers.len()
            );
            Some(c)
        }
        Err(e) => {
            println!("config: {e:#}");
            None
        }
    };
    let ignore = cfg
        .as_ref()
        .map(|c| c.ignore_devices.clone())
        .unwrap_or_default();
    println!("\ninput devices:");
    for p in devices::probe_all(&ignore) {
        match p.verdict {
            Ok(d) => {
                let kinds: Vec<&str> = [
                    (d.keyboard, "keyboard"),
                    (d.pointer, "mouse"),
                    (d.touchpad, "touchpad"),
                    (d.gamepad && !d.rumble, "gamepad"),
                    (d.gamepad && d.rumble, "gamepad+rumble"),
                ]
                .iter()
                .filter(|(on, _)| *on)
                .map(|(_, k)| *k)
                .collect();
                println!(
                    "  capture  {:<14} {} [{}]",
                    p.path.display(),
                    p.name,
                    kinds.join("+")
                );
            }
            Err(why) => println!("  skip     {:<14} {} ({why})", p.path.display(), p.name),
        }
    }
    let uinput = std::fs::OpenOptions::new().write(true).open("/dev/uinput");
    println!(
        "\n/dev/uinput: {}",
        if uinput.is_ok() {
            "writable"
        } else {
            "NOT writable (add a udev rule / input group)"
        }
    );
    let mut h = hypr::Hypr::new();
    match (h.layout().await, h.cursor().await) {
        (Ok(l), Ok((x, y))) => {
            println!("\nscreens (logical px):");
            for m in &l.monitors {
                println!("  {}x{} at {},{}", m.w, m.h, m.x, m.y);
            }
            println!("cursor: {x},{y}");
            if let Some(cfg) = &cfg {
                for p in &cfg.peers {
                    println!(
                        "  {} is {:?}: cursor is {:.0} px from that edge",
                        p.name,
                        p.side,
                        l.distance_to(p.side, x, y)
                    );
                }
            }
        }
        (Err(e), _) | (_, Err(e)) => println!("\nhyprland: {e:#}"),
    }
    Ok(())
}
