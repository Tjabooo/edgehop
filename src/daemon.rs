//! The state machine: watch the cursor, hand input over at screen edges,
//! replay input from peers, hand it back.

use std::collections::{BTreeSet, HashMap};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::Config;
use crate::devices::{self, DevEvent, InputState};
use crate::geometry::Side;
use crate::hypr::Hypr;
use crate::mirror::Mirror;
use crate::proto::{self, DeviceDesc, Ev, Msg, PROTOCOL_VERSION};
use crate::view::{self, Task};

const PING_EVERY: Duration = Duration::from_secs(1);
const DEAD_AFTER: Duration = Duration::from_secs(4);
const REDIAL_EVERY: Duration = Duration::from_secs(2);

enum NetEvent {
    Up {
        peer: String,
        conn: u64,
        tx: mpsc::UnboundedSender<Msg>,
        initiator: bool,
    },
    /// A remote-view video link came up (dialed by us or by the peer).
    VideoLink {
        peer: String,
        session: u64,
        r: proto::Reader,
        w: proto::Writer,
    },
    /// A remote-view stream (either role) stopped by itself.
    ViewEnded {
        session: u64,
        why: String,
    },
    Msg {
        peer: String,
        conn: u64,
        msg: Msg,
    },
    Down {
        peer: String,
        conn: u64,
    },
}

struct Control {
    cmd: String,
    reply: oneshot::Sender<String>,
}

struct LocalDev {
    desc: DeviceDesc,
    grab_fd: OwnedFd,
    state: InputState,
    path: PathBuf,
}

struct Link {
    conn: u64,
    tx: mpsc::UnboundedSender<Msg>,
    /// We dialed this link; video links get dialed the same way.
    initiator: bool,
}

/// Viewer: we're showing `peer`'s screen.
struct Viewing {
    peer: String,
    session: u64,
    task: Option<Task>,
}

/// Watched: we're streaming our screen to `peer`.
struct Serving {
    peer: String,
    session: u64,
    capture: Vec<String>,
    monitor: watch::Sender<String>,
    task: Option<Task>,
}

const FOLLOW_EVERY: Duration = Duration::from_millis(40);
const FOLLOW_TICK: Duration = Duration::from_millis(100);

/// Rate-limits cursor queries: often near an edge, rarely far from it.
struct EdgeWatch {
    next: Instant,
}

impl EdgeWatch {
    fn new() -> EdgeWatch {
        EdgeWatch {
            next: Instant::now(),
        }
    }
    fn due(&self) -> bool {
        Instant::now() >= self.next
    }
    fn after_query(&mut self, distance: f64) {
        let wait = if distance < 48.0 {
            2
        } else if distance < 400.0 {
            8
        } else {
            30
        };
        self.next = Instant::now() + Duration::from_millis(wait);
    }
}

struct Controlling {
    peer: String,
    side: Side,
    /// Keeps hypridle from locking/suspending this machine while its own
    /// input is grabbed (it sees no activity).
    inhibit: Option<tokio::process::Child>,
}

struct ControlledBy {
    peer: String,
    side: Side,
}

struct Daemon {
    cfg: Config,
    release_codes: Vec<u16>,
    hypr: Hypr,
    devices: HashMap<u32, LocalDev>,
    links: HashMap<String, Link>,
    mirrors: HashMap<(String, u32), Mirror>,
    controlling: Option<Controlling>,
    controlled_by: Option<ControlledBy>,
    enabled: bool,
    out_watch: EdgeWatch,
    back_watch: EdgeWatch,
    /// `edgehop switch` waits for the keybind's keys to be let go first,
    /// else this machine's compositor would never see them released.
    pending_switch: Option<(String, Side, Instant)>,
    notifier: crate::notify::Notifier,
    /// Edge delay: (peer, first push against the edge, latest push).
    edge_push: Option<(String, Instant, Instant)>,
    ways_back: String,
    psk: [u8; 32],
    net_tx: mpsc::UnboundedSender<NetEvent>,
    /// Remote view switched on: show the screen of whoever we drive.
    view_on: bool,
    viewing: Option<Viewing>,
    serving: Option<Serving>,
    /// Next time the watched side checks which output the cursor is on.
    follow_next: Instant,
}

pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("EDGEHOP_SOCKET") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    base.join("edgehop.sock")
}

pub async fn run(cfg: Config) -> Result<()> {
    let psk = cfg.psk_bytes()?;
    let (dev_tx, mut dev_rx) = mpsc::channel::<DevEvent>(1024);
    let (net_tx, mut net_rx) = mpsc::unbounded_channel::<NetEvent>();
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<Control>(8);

    tokio::spawn(devices::manager(cfg.ignore_devices.clone(), dev_tx));

    let peer_names: Vec<String> = cfg.peers.iter().map(|p| p.name.clone()).collect();
    if let Some(addr) = cfg.listen.as_deref().map(crate::config::with_port) {
        let addr = &addr;
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listen on {addr}"))?;
        log::info!("listening on {addr}");
        let (net_tx, names, me) = (net_tx.clone(), peer_names.clone(), cfg.name.clone());
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((s, from)) => {
                        log::debug!("connection from {from}");
                        tokio::spawn(connection(
                            s,
                            psk,
                            false,
                            me.clone(),
                            names.clone(),
                            None,
                            net_tx.clone(),
                        ));
                    }
                    Err(e) => {
                        log::warn!("accept: {e}");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        });
    }
    for p in &cfg.peers {
        let Some(addr) = p.address.as_deref().map(crate::config::with_port) else {
            continue;
        };
        let (net_tx, names, me, want) = (
            net_tx.clone(),
            peer_names.clone(),
            cfg.name.clone(),
            p.name.clone(),
        );
        tokio::spawn(async move {
            let mut last_err = String::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr)).await
                {
                    Ok(Ok(s)) => {
                        last_err.clear();
                        connection(
                            s,
                            psk,
                            true,
                            me.clone(),
                            names.clone(),
                            Some(want.clone()),
                            net_tx.clone(),
                        )
                        .await;
                    }
                    Ok(Err(e)) => {
                        let e = e.to_string();
                        if e != last_err {
                            log::info!("dial {want} at {addr}: {e} (retrying)");
                            last_err = e;
                        }
                    }
                    Err(_) => {
                        if last_err != "timeout" {
                            log::info!("dial {want} at {addr}: timed out (retrying)");
                            last_err = "timeout".into();
                        }
                    }
                }
                tokio::time::sleep(REDIAL_EVERY).await;
            }
        });
    }

    let sock = socket_path();
    let _ = std::fs::remove_file(&sock);
    let ctl = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    tokio::spawn(async move {
        while let Ok((s, _)) = ctl.accept().await {
            let ctl_tx = ctl_tx.clone();
            tokio::spawn(async move {
                let (r, mut w) = s.into_split();
                let mut line = String::new();
                if BufReader::new(r).read_line(&mut line).await.is_err() {
                    return;
                }
                let (reply, rx) = oneshot::channel();
                if ctl_tx
                    .send(Control {
                        cmd: line.trim().to_string(),
                        reply,
                    })
                    .await
                    .is_ok()
                    && let Ok(text) = rx.await
                {
                    let _ = w.write_all(text.as_bytes()).await;
                }
            });
        }
    });

    let notifier = crate::notify::Notifier::new(cfg.notify);
    let ways_back = crate::notify::ways_back(&cfg.release_keys, cfg.switch_hint.as_deref());
    let mut d = Daemon {
        release_codes: cfg.release_codes()?,
        cfg,
        hypr: Hypr::new(),
        devices: HashMap::new(),
        links: HashMap::new(),
        mirrors: HashMap::new(),
        controlling: None,
        controlled_by: None,
        enabled: true,
        out_watch: EdgeWatch::new(),
        back_watch: EdgeWatch::new(),
        pending_switch: None,
        notifier,
        edge_push: None,
        ways_back,
        psk,
        net_tx: net_tx.clone(),
        view_on: false,
        viewing: None,
        serving: None,
        follow_next: Instant::now(),
    };
    d.view_on = d.cfg.view.on;

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // While streaming, also catch cursor jumps that came without pointer
    // motion (a keybind focusing another monitor).
    let mut follow = tokio::time::interval(FOLLOW_TICK);
    follow.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = follow.tick(), if d.serving.is_some() => d.follow_cursor().await,
            Some(ev) = dev_rx.recv() => d.on_device(ev).await,
            Some(ev) = net_rx.recv() => d.on_net(ev).await,
            Some(c) = ctl_rx.recv() => {
                let text = d.on_control(&c.cmd).await;
                let _ = c.reply.send(text);
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        }
    }
    d.stop_control("shutting down", None).await;
    d.serving = None;
    d.mirrors.clear();
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

/// One authenticated connection to a peer: handshake, hello, then pump
/// messages both ways until it dies.
async fn connection(
    stream: TcpStream,
    psk: [u8; 32],
    initiator: bool,
    me: String,
    peers: Vec<String>,
    want: Option<String>,
    net: mpsc::UnboundedSender<NetEvent>,
) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let conn = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let from = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let setup = async {
        let (mut r, mut w) = proto::handshake(stream, &psk, initiator).await?;
        w.send(&Msg::Hello {
            name: me.clone(),
            version: PROTOCOL_VERSION,
            video: None,
        })
        .await?;
        let Msg::Hello {
            name,
            version,
            video,
        } = r.recv().await?
        else {
            bail!("expected hello")
        };
        if version != PROTOCOL_VERSION {
            bail!("{name} speaks protocol {version}, this is {PROTOCOL_VERSION}: update both");
        }
        if !peers.contains(&name) {
            bail!("{name:?} is not in this machine's peers");
        }
        if want.as_ref().is_some_and(|w| *w != name) {
            bail!("expected {:?}, got {name:?}", want.unwrap());
        }
        Ok((r, w, name, video))
    };
    let (mut r, mut w, peer, video) =
        match tokio::time::timeout(Duration::from_secs(5), setup).await {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                log::warn!("connection {from}: {e:#}");
                return;
            }
            Err(_) => {
                log::warn!("connection {from}: handshake timed out");
                return;
            }
        };
    if let Some(session) = video {
        log::debug!("{peer}: video link for view {session:x}");
        let _ = net.send(NetEvent::VideoLink {
            peer,
            session,
            r,
            w,
        });
        return;
    }
    log::info!("connected to {peer} ({from})");
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    if net
        .send(NetEvent::Up {
            peer: peer.clone(),
            conn,
            tx,
            initiator,
        })
        .is_err()
    {
        return;
    }
    let writer = tokio::spawn(async move {
        let mut tick = tokio::time::interval(PING_EVERY);
        loop {
            let msg = tokio::select! {
                m = rx.recv() => match m { Some(m) => m, None => return },
                _ = tick.tick() => Msg::Ping,
            };
            if let Err(e) = w.send(&msg).await {
                log::debug!("send: {e}");
                return;
            }
        }
    });
    loop {
        match tokio::time::timeout(DEAD_AFTER, r.recv()).await {
            Ok(Ok(Msg::Ping)) => {}
            Ok(Ok(msg)) => {
                if net
                    .send(NetEvent::Msg {
                        peer: peer.clone(),
                        conn,
                        msg,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(Err(e)) => {
                log::info!("{peer}: connection closed ({e:#})");
                break;
            }
            Err(_) => {
                log::info!("{peer}: no answer for {}s, dropping", DEAD_AFTER.as_secs());
                break;
            }
        }
        if writer.is_finished() {
            break;
        }
    }
    writer.abort();
    let _ = net.send(NetEvent::Down { peer, conn });
}

impl Daemon {
    fn send(&self, peer: &str, msg: Msg) {
        if let Some(l) = self.links.get(peer) {
            let _ = l.tx.send(msg);
        }
    }

    // ---------------------------------------------------------------- local input

    async fn on_device(&mut self, ev: DevEvent) {
        match ev {
            DevEvent::Added {
                id,
                desc,
                grab_fd,
                path,
            } => {
                for peer in self.links.keys() {
                    self.send(
                        peer,
                        Msg::AddDevice {
                            id,
                            desc: desc.clone(),
                        },
                    );
                }
                if self.controlling.is_some()
                    && let Err(e) = devices::set_grab(&grab_fd, true)
                {
                    log::warn!("grab {}: {e}", desc.name);
                }
                self.devices.insert(
                    id,
                    LocalDev {
                        desc,
                        grab_fd,
                        state: InputState::new(),
                        path,
                    },
                );
            }
            DevEvent::Removed { id } => {
                self.devices.remove(&id);
                for peer in self.links.keys() {
                    self.send(peer, Msg::RemoveDevice { id });
                }
            }
            DevEvent::Batch { id, events } => self.on_batch(id, events).await,
        }
    }

    async fn on_batch(&mut self, id: u32, events: Vec<Ev>) {
        let Some(dev) = self.devices.get_mut(&id) else {
            return;
        };
        let (dx, dy) = dev.state.apply(&events);
        let is_pointer = dev.desc.pointer || dev.desc.touchpad;

        if let Some(c) = &self.controlling {
            let peer = c.peer.clone();
            if self.release_combo_held() {
                log::info!("release keys pressed");
                self.send(&peer, Msg::Leave);
                self.stop_control("release keys", None).await;
                return;
            }
            self.send(&peer, Msg::Input { id, events });
            return;
        }
        if self.pending_switch.is_some() {
            self.try_pending_switch().await;
            if self.controlling.is_some() {
                return;
            }
        }
        if !is_pointer || (dx == 0.0 && dy == 0.0) || !self.enabled || self.controlled_by.is_some()
        {
            return;
        }
        // Which connected peers is this motion heading toward?
        let targets: Vec<(String, Side)> = self
            .cfg
            .peers
            .iter()
            .filter(|p| self.links.contains_key(&p.name) && p.side.pushed_by(dx, dy))
            .map(|p| (p.name.clone(), p.side))
            .collect();
        if targets.is_empty() || !self.out_watch.due() {
            return;
        }
        let (Ok((x, y)), Ok(layout)) = (self.hypr.cursor().await, self.hypr.layout().await) else {
            self.out_watch.after_query(1000.0);
            return;
        };
        let dist = targets
            .iter()
            .map(|(p, s)| layout.sub(&self.peer_monitors(p)).distance_to(*s, x, y))
            .fold(f64::MAX, f64::min);
        self.out_watch.after_query(dist);
        for (peer, side) in targets {
            let Some(frac) = layout.at_edge_on(side, x, y, &self.peer_monitors(&peer)) else {
                continue;
            };
            if self.devices.values().any(|d| d.state.anything_held()) {
                log::debug!("at {side:?} edge but a key/button is held");
                return;
            }
            if self.cfg.block_when_fullscreen && self.hypr.fullscreen().await {
                log::debug!("at {side:?} edge but a fullscreen window is focused");
                return;
            }
            // Edge delay: keep pushing for a moment before crossing.
            let delay = self
                .cfg
                .peers
                .iter()
                .find(|p| p.name == peer)
                .map_or(0, |p| p.edge_delay_ms);
            if delay > 0 {
                let now = Instant::now();
                let first = match &self.edge_push {
                    Some((p, first, last))
                        if *p == peer && now.duration_since(*last) < Duration::from_millis(120) =>
                    {
                        *first
                    }
                    _ => now,
                };
                self.edge_push = Some((peer.clone(), first, now));
                if now.duration_since(first) < Duration::from_millis(delay) {
                    return;
                }
            }
            self.edge_push = None;
            self.start_control(peer, side, frac).await;
            return;
        }
        // Not at any peer's edge (every crossing path above returns).
        self.edge_push = None;
    }

    fn peer_monitors(&self, peer: &str) -> Vec<String> {
        self.cfg
            .peers
            .iter()
            .find(|p| p.name == peer)
            .map(|p| p.monitors.clone())
            .unwrap_or_default()
    }

    fn release_combo_held(&self) -> bool {
        if self.release_codes.is_empty() {
            return false;
        }
        let held: BTreeSet<u16> = self
            .devices
            .values()
            .flat_map(|d| d.state.keys.iter().copied())
            .collect();
        self.release_codes.iter().all(|k| held.contains(k))
    }

    async fn start_control(&mut self, peer: String, side: Side, frac: f64) {
        log::info!("→ controlling {peer} ({side:?} edge, {:.0}%)", frac * 100.0);
        for d in self.devices.values() {
            if let Err(e) = devices::set_grab(&d.grab_fd, true) {
                log::warn!("grab {} ({}): {e}", d.desc.name, d.path.display());
            }
        }
        self.send(
            &peer,
            Msg::Enter {
                side: side.opposite(),
                frac,
            },
        );
        // Carry an ongoing swipe over to the other side.
        for (id, d) in &self.devices {
            if d.desc.touchpad {
                let resume = d.state.touch_resume();
                if !resume.is_empty() {
                    self.send(
                        &peer,
                        Msg::Input {
                            id: *id,
                            events: resume,
                        },
                    );
                }
            }
        }
        let inhibit = tokio::process::Command::new("systemd-inhibit")
            .args([
                "--what=idle:sleep",
                "--who=edgehop",
                "--why=Controlling another machine",
                "--mode=block",
            ])
            .args(["sleep", "infinity"])
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| log::debug!("systemd-inhibit: {e}"))
            .ok();
        self.notifier
            .driving(&peer, side.opposite(), &self.ways_back, self.view_on);
        self.controlling = Some(Controlling {
            peer,
            side,
            inhibit,
        });
        if self.view_on {
            self.begin_view();
        }
    }

    /// Give local input back. `back_at`: where along our edge the cursor returns.
    async fn stop_control(&mut self, why: &str, back_at: Option<f64>) {
        self.end_view();
        let Some(mut c) = self.controlling.take() else {
            return;
        };
        for d in self.devices.values() {
            if let Err(e) = devices::set_grab(&d.grab_fd, false) {
                log::warn!("ungrab {}: {e}", d.desc.name);
            }
            if d.desc.touchpad
                && let Err(e) = devices::resync_touchpad(&d.path, &d.grab_fd, &d.desc.keys)
            {
                log::warn!("resync {}: {e}", d.desc.name);
            }
        }
        if let Some(mut child) = c.inhibit.take() {
            let _ = child.start_kill();
        }
        log::info!("← back from {} ({why})", c.peer);
        self.notifier.back(&self.cfg.name);
        if let Some(frac) = back_at
            && let Ok(layout) = self.hypr.layout().await
        {
            let (x, y) = layout
                .sub(&self.peer_monitors(&c.peer))
                .entry_point(c.side, frac);
            if let Err(e) = self.hypr.warp(x, y).await {
                log::warn!("{e:#}");
            }
        }
    }

    // ---------------------------------------------------------------- network

    async fn on_net(&mut self, ev: NetEvent) {
        match ev {
            NetEvent::Up {
                peer,
                conn,
                tx,
                initiator,
            } => {
                if self.links.contains_key(&peer) {
                    log::info!("{peer}: replacing older connection");
                    self.peer_gone(&peer).await;
                }
                for (id, d) in &self.devices {
                    let _ = tx.send(Msg::AddDevice {
                        id: *id,
                        desc: d.desc.clone(),
                    });
                }
                self.links.insert(
                    peer,
                    Link {
                        conn,
                        tx,
                        initiator,
                    },
                );
            }
            NetEvent::VideoLink {
                peer,
                session,
                r,
                w,
            } => self.on_video_link(&peer, session, r, w),
            NetEvent::ViewEnded { session, why } => self.on_view_ended(session, &why),
            NetEvent::Down { peer, conn } => {
                if self.links.get(&peer).is_some_and(|l| l.conn == conn) {
                    log::info!("{peer} disconnected");
                    self.peer_gone(&peer).await;
                }
            }
            NetEvent::Msg { peer, conn, msg } => {
                if self.links.get(&peer).is_some_and(|l| l.conn == conn) {
                    self.on_msg(&peer, msg).await;
                }
            }
        }
    }

    async fn peer_gone(&mut self, peer: &str) {
        self.links.remove(peer);
        if self.controlling.as_ref().is_some_and(|c| c.peer == peer) {
            self.stop_control("peer disconnected", None).await;
        }
        if self.controlled_by.as_ref().is_some_and(|c| c.peer == peer) {
            self.controlled_by = None;
        }
        if self.viewing.as_ref().is_some_and(|v| v.peer == peer) {
            self.viewing = None;
        }
        if self.serving.as_ref().is_some_and(|v| v.peer == peer) {
            self.serving = None;
        }
        self.mirrors.retain(|(p, _), _| p != peer); // Drop lifts held keys
    }

    async fn on_msg(&mut self, peer: &str, msg: Msg) {
        match msg {
            Msg::Hello { .. } | Msg::Ping => {}
            Msg::AddDevice { id, desc } => {
                let name = desc.name.clone();
                match Mirror::create(desc) {
                    Ok(m) => {
                        log::info!("{peer}: mirrored device {id} {name}");
                        self.mirrors.insert((peer.to_string(), id), m);
                    }
                    Err(e) => log::warn!("{peer}: mirror {name}: {e:#}"),
                }
            }
            Msg::RemoveDevice { id } => {
                self.mirrors.remove(&(peer.to_string(), id));
            }
            Msg::Enter { side, frac } => {
                if self.controlling.is_some() || !self.enabled {
                    // Busy (we're driving someone else) or switched off: bounce.
                    self.send(peer, Msg::Released { frac });
                    return;
                }
                log::info!("← {peer} took over ({side:?} edge, {:.0}%)", frac * 100.0);
                self.notifier
                    .arrived(peer, &self.cfg.name, side, &self.ways_back);
                self.controlled_by = Some(ControlledBy {
                    peer: peer.to_string(),
                    side,
                });
                self.back_watch = EdgeWatch::new();
                match self.hypr.layout().await {
                    Ok(layout) => {
                        let (x, y) = layout
                            .sub(&self.peer_monitors(peer))
                            .entry_point(side, frac);
                        if let Err(e) = self.hypr.warp(x, y).await {
                            log::warn!("{e:#}");
                        }
                    }
                    Err(e) => log::warn!("layout: {e:#}"),
                }
            }
            Msg::Input { id, events } => {
                let Some(cb) = &self.controlled_by else {
                    return;
                };
                if cb.peer != peer {
                    return;
                }
                let side = cb.side;
                let Some(m) = self.mirrors.get_mut(&(peer.to_string(), id)) else {
                    return;
                };
                let pointer = m.desc.pointer || m.desc.touchpad;
                let (dx, dy) = match m.emit(&events) {
                    Ok(d) => d,
                    Err(e) => {
                        log::warn!("inject: {e}");
                        return;
                    }
                };
                if pointer
                    && (dx != 0.0 || dy != 0.0)
                    && self.serving.is_some()
                    && Instant::now() >= self.follow_next
                {
                    self.follow_next = Instant::now() + FOLLOW_EVERY;
                    self.follow_cursor().await;
                }
                if pointer && side.pushed_by(dx, dy) && self.back_watch.due() {
                    self.check_return(peer, side).await;
                }
            }
            Msg::Leave => {
                if self.controlled_by.as_ref().is_some_and(|c| c.peer == peer) {
                    log::info!("→ {peer} let go");
                    self.release_mirrors(peer);
                    self.controlled_by = None;
                    self.serving = None;
                }
            }
            Msg::Released { frac } => {
                if self.controlling.as_ref().is_some_and(|c| c.peer == peer) {
                    self.stop_control("cursor came back", Some(frac)).await;
                }
            }
            Msg::ViewToggle { on } => {
                if self.controlling.as_ref().is_some_and(|c| c.peer == peer) {
                    self.set_view(on);
                }
            }
            Msg::ViewStart {
                session,
                fps,
                kbps,
                codec,
            } => self.on_view_start(peer, session, fps, kbps, &codec).await,
            Msg::ViewStop { session } => {
                if self.serving.as_ref().is_some_and(|s| s.session == session) {
                    log::info!("view: {peer} stopped watching");
                    self.serving = None;
                }
            }
            Msg::ViewFailed { session, why } => {
                if self.viewing.as_ref().is_some_and(|v| v.session == session) {
                    self.view_broke(&format!("{peer}: {why}"));
                }
            }
            Msg::ViewData(_) => {}
        }
    }

    /// Receiver: has the cursor been pushed out through the edge facing
    /// the controller? Then hand control back.
    async fn check_return(&mut self, peer: &str, side: Side) {
        let (Ok((x, y)), Ok(layout)) = (self.hypr.cursor().await, self.hypr.layout().await) else {
            self.back_watch.after_query(1000.0);
            return;
        };
        let only = self.peer_monitors(peer);
        let dist = layout.sub(&only).distance_to(side, x, y);
        self.back_watch.after_query(dist);
        log::debug!("return check: cursor {x},{y}, {dist:.0} px from the {side:?} edge");
        if let Some(frac) = layout.at_edge_on(side, x, y, &only) {
            log::info!("→ handing back to {peer}");
            self.release_mirrors(peer);
            self.controlled_by = None;
            self.serving = None;
            self.send(peer, Msg::Released { frac });
        }
    }

    fn release_mirrors(&mut self, peer: &str) {
        for ((p, _), m) in self.mirrors.iter_mut() {
            if p == peer {
                m.release_all();
            }
        }
    }

    // ---------------------------------------------------------------- control socket

    async fn try_pending_switch(&mut self) {
        let Some((peer, side, deadline)) = self.pending_switch.clone() else {
            return;
        };
        if Instant::now() > deadline
            || !self.links.contains_key(&peer)
            || self.controlled_by.is_some()
        {
            self.pending_switch = None;
            return;
        }
        if self.devices.values().any(|d| d.state.anything_held()) {
            return;
        }
        self.pending_switch = None;
        let frac = match (self.hypr.cursor().await, self.hypr.layout().await) {
            (Ok((x, y)), Ok(l)) => l.sub(&self.peer_monitors(&peer)).frac_of(side, x, y),
            _ => 0.5,
        };
        self.start_control(peer, side, frac).await;
    }

    /// `edgehop switch [peer]`: jump to a peer (or back home) without the edge.
    async fn switch(&mut self, want: Option<&str>) -> Result<(), String> {
        if let Some(cb) = self.controlled_by.take() {
            // Pressed on the controller's keyboard, run here: hand control back.
            let frac = match (self.hypr.cursor().await, self.hypr.layout().await) {
                (Ok((x, y)), Ok(l)) => l.sub(&self.peer_monitors(&cb.peer)).frac_of(cb.side, x, y),
                _ => 0.5,
            };
            log::info!("→ handing back to {} (switch)", cb.peer);
            self.serving = None;
            self.release_mirrors(&cb.peer);
            self.send(&cb.peer, Msg::Released { frac });
            return Ok(());
        }
        if let Some(c) = &self.controlling {
            let peer = c.peer.clone();
            self.send(&peer, Msg::Leave);
            self.stop_control("edgehop switch", None).await;
            return Ok(());
        }
        let peer = self
            .cfg
            .peers
            .iter()
            .find(|p| want.is_none_or(|w| w == p.name) && self.links.contains_key(&p.name))
            .map(|p| (p.name.clone(), p.side));
        let Some((peer, side)) = peer else {
            return Err(match want {
                Some(w) => format!("{w} is not connected (or not a peer)"),
                None => "no peer is connected".into(),
            });
        };
        self.pending_switch = Some((peer, side, Instant::now() + Duration::from_secs(3)));
        self.try_pending_switch().await;
        Ok(())
    }

    async fn on_control(&mut self, cmd: &str) -> String {
        let (verb, arg) = match cmd.split_once(' ') {
            Some((v, a)) => (v, Some(a.trim()).filter(|a| !a.is_empty())),
            None => (cmd, None),
        };
        if verb == "view" {
            let on = match arg {
                None | Some("toggle") => None,
                Some("on") => Some(true),
                Some("off") => Some(false),
                Some(other) => {
                    return format!("error: view takes on, off or toggle, not {other:?}\n");
                }
            };
            if let Some(cb) = &self.controlled_by {
                // Pressed on the driver's keyboard: remote view is its setting.
                let peer = cb.peer.clone();
                self.send(&peer, Msg::ViewToggle { on });
                return format!("asked {peer} to switch remote view\n");
            }
            self.set_view(on);
            return self.status();
        }
        if verb == "switch" {
            if let Err(e) = self.switch(arg).await {
                return format!("error: {e}\n");
            }
            return self.status();
        }
        match cmd {
            "status" | "" => {}
            "toggle" => self.enabled = !self.enabled,
            "enable" | "on" => self.enabled = true,
            "disable" | "off" => self.enabled = false,
            "release" => {
                if let Some(c) = &self.controlling {
                    let peer = c.peer.clone();
                    self.send(&peer, Msg::Leave);
                }
                self.stop_control("edgehop release", None).await;
            }
            other => {
                return format!(
                    "unknown command {other:?} (status, switch [peer], view [on|off], toggle, enable, disable, release)\n"
                );
            }
        }
        if matches!(cmd, "toggle" | "enable" | "on" | "disable" | "off") && !self.enabled {
            if let Some(c) = &self.controlling {
                let peer = c.peer.clone();
                self.send(&peer, Msg::Leave);
            }
            self.stop_control("disabled", None).await;
        }
        self.status()
    }

    // ---------------------------------------------------------------- remote view

    /// Flip (None) or set remote view; opens/closes it right away while driving.
    fn set_view(&mut self, on: Option<bool>) {
        let on = on.unwrap_or(!self.view_on);
        self.view_on = on;
        log::info!("remote view {}", if on { "on" } else { "off" });
        let peer = self.controlling.as_ref().map(|c| c.peer.clone());
        self.notifier
            .view_switched(on, peer.as_deref(), self.cfg.view.hint.as_deref());
        if on {
            self.begin_view();
        } else {
            self.end_view();
        }
    }

    /// Viewer: ask the machine we drive for its screen.
    fn begin_view(&mut self) {
        let Some(c) = &self.controlling else {
            return;
        };
        if self.viewing.is_some() {
            return;
        }
        let peer = c.peer.clone();
        let session = view::session_id();
        let v = &self.cfg.view;
        log::info!(
            "view: asking {peer} for its screen ({} fps, {} kb/s {})",
            v.fps,
            v.bitrate_kbps,
            v.codec
        );
        self.send(
            &peer,
            Msg::ViewStart {
                session,
                fps: v.fps,
                kbps: v.bitrate_kbps,
                codec: v.codec.clone(),
            },
        );
        self.viewing = Some(Viewing {
            peer: peer.clone(),
            session,
            task: None,
        });
        self.dial_video(&peer, session);
    }

    fn end_view(&mut self) {
        if let Some(v) = self.viewing.take() {
            log::info!("view: closed {}", v.peer);
            self.send(&v.peer, Msg::ViewStop { session: v.session });
        }
    }

    /// The viewer's stream died on its own: say why, keep remote view on.
    fn view_broke(&mut self, why: &str) {
        let Some(v) = self.viewing.take() else {
            return;
        };
        log::warn!("view: {why}");
        self.send(&v.peer, Msg::ViewStop { session: v.session });
        self.notifier.view_failed(&v.peer, why);
    }

    /// Whoever dialed the main link dials the video link too (the other side
    /// may not accept connections at all).
    fn dial_video(&self, peer: &str, session: u64) {
        if !self.links.get(peer).is_some_and(|l| l.initiator) {
            return; // the peer dials us
        }
        let Some(addr) = self
            .cfg
            .peers
            .iter()
            .find(|p| p.name == peer)
            .and_then(|p| p.address.as_deref())
            .map(crate::config::with_port)
        else {
            return;
        };
        let (net, psk, me, want) = (
            self.net_tx.clone(),
            self.psk,
            self.cfg.name.clone(),
            peer.to_string(),
        );
        tokio::spawn(async move {
            let ev = match view::dial(&addr, psk, &me, &want, session).await {
                Ok((r, w)) => NetEvent::VideoLink {
                    peer: want,
                    session,
                    r,
                    w,
                },
                Err(e) => NetEvent::ViewEnded {
                    session,
                    why: format!("video link to {want}: {e:#}"),
                },
            };
            let _ = net.send(ev);
        });
    }

    /// Watched: the machine driving us wants to see our screen.
    async fn on_view_start(&mut self, peer: &str, session: u64, fps: u32, kbps: u32, codec: &str) {
        if !self.controlled_by.as_ref().is_some_and(|c| c.peer == peer) {
            self.send(
                peer,
                Msg::ViewFailed {
                    session,
                    why: "not being driven from there".into(),
                },
            );
            return;
        }
        if !crate::config::valid_codec(codec) {
            self.send(
                peer,
                Msg::ViewFailed {
                    session,
                    why: format!("unknown codec {codec:?}"),
                },
            );
            return;
        }
        let monitor = self.monitor_under_cursor().await.unwrap_or_default();
        let capture = crate::config::fill(
            &self.cfg.view.capture,
            &[
                ("fps", &fps.to_string()),
                ("kbps", &kbps.to_string()),
                ("codec", codec),
            ],
        );
        log::info!("view: {peer} is watching (starting on {monitor})");
        let (tx, _) = watch::channel(monitor);
        self.serving = Some(Serving {
            peer: peer.to_string(),
            session,
            capture,
            monitor: tx,
            task: None,
        });
        self.follow_next = Instant::now() + FOLLOW_EVERY;
        self.dial_video(peer, session);
    }

    async fn monitor_under_cursor(&mut self) -> Option<String> {
        let (x, y) = self.hypr.cursor().await.ok()?;
        let layout = self.hypr.layout().await.ok()?;
        layout
            .monitor_at(x, y)
            .or(layout.names.first().map(String::as_str))
            .map(String::from)
    }

    /// Watched: switch the stream to the output the cursor moved onto.
    async fn follow_cursor(&mut self) {
        let Some(now) = self.monitor_under_cursor().await else {
            return;
        };
        if let Some(sv) = &self.serving
            && *sv.monitor.borrow() != now
        {
            log::debug!("view: cursor moved to {now}");
            sv.monitor.send_replace(now);
        }
    }

    fn on_video_link(&mut self, peer: &str, session: u64, r: proto::Reader, w: proto::Writer) {
        let ended = move |net: mpsc::UnboundedSender<NetEvent>, res: anyhow::Result<()>| {
            let why = match res {
                Ok(()) => "the stream ended".to_string(),
                Err(e) => format!("{e:#}"),
            };
            let _ = net.send(NetEvent::ViewEnded { session, why });
        };
        let net = self.net_tx.clone();
        if let Some(v) = self
            .viewing
            .as_mut()
            .filter(|v| v.peer == peer && v.session == session && v.task.is_none())
        {
            let player = crate::config::fill(
                &self.cfg.view.player,
                &[("codec", &self.cfg.view.codec), ("peer", peer)],
            );
            log::info!("view: showing {peer}");
            v.task = Some(Task::spawn(async move {
                ended(net, view::watch(r, w, player).await);
            }));
        } else if let Some(sv) = self
            .serving
            .as_mut()
            .filter(|s| s.peer == peer && s.session == session && s.task.is_none())
        {
            let (capture, monitor) = (sv.capture.clone(), sv.monitor.subscribe());
            sv.task = Some(Task::spawn(async move {
                ended(net, view::serve(w, r, capture, monitor).await);
            }));
        } else {
            log::debug!("{peer}: video link for unknown view {session:x}, dropped");
        }
    }

    fn on_view_ended(&mut self, session: u64, why: &str) {
        if self.viewing.as_ref().is_some_and(|v| v.session == session) {
            self.view_broke(why);
        } else if let Some(sv) = self.serving.take_if(|s| s.session == session) {
            log::warn!("view: streaming to {} stopped: {why}", sv.peer);
            self.send(
                &sv.peer,
                Msg::ViewFailed {
                    session,
                    why: why.to_string(),
                },
            );
        }
    }

    fn status(&self) -> String {
        let mut s = String::new();
        s += &format!(
            "name: {}\nenabled: {}\n",
            self.cfg.name,
            if self.enabled { "yes" } else { "no" }
        );
        for p in &self.cfg.peers {
            let state = if self.links.contains_key(&p.name) {
                "connected"
            } else {
                "not connected"
            };
            s += &format!("peer: {} ({}, {state})\n", p.name, side_word(p.side));
        }
        let mode = match (&self.controlling, &self.controlled_by) {
            (Some(c), _) => format!("controlling {}", c.peer),
            (_, Some(c)) => format!("controlled by {}", c.peer),
            _ => "local".to_string(),
        };
        s += &format!("state: {mode}\n");
        s += &format!("remote view: {}", if self.view_on { "on" } else { "off" });
        if let Some(v) = &self.viewing {
            s += &format!(
                " (watching {}{})",
                v.peer,
                if v.task.is_some() { "" } else { ", connecting" }
            );
        }
        if let Some(sv) = &self.serving {
            s += &format!(
                ", streaming {} to {}",
                sv.monitor.borrow().as_str(),
                sv.peer
            );
        }
        s += "\n";
        let mut names: Vec<&str> = self
            .devices
            .values()
            .map(|d| d.desc.name.as_str())
            .collect();
        names.sort();
        s += &format!("captured devices: {}\n", names.join(", "));
        let mirrored = self.mirrors.len();
        if mirrored > 0 {
            s += &format!("mirrored devices: {mirrored}\n");
        }
        s
    }
}

fn side_word(s: Side) -> &'static str {
    match s {
        Side::Left => "left",
        Side::Right => "right",
        Side::Top => "above",
        Side::Bottom => "below",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Ev;

    /// Live check against a running daemon that lists a peer named "probe"
    /// on its `EDGEHOP_PROBE_SIDE` side (default left). Moves the real cursor:
    /// EDGEHOP_PROBE_ADDR=host:port EDGEHOP_PROBE_PSK=… cargo test probe -- --ignored
    #[tokio::test]
    #[ignore]
    async fn probe_enter_move_and_return() {
        let addr = std::env::var("EDGEHOP_PROBE_ADDR").unwrap();
        let psk = crate::config::Config {
            name: String::new(),
            listen: None,
            psk: std::env::var("EDGEHOP_PROBE_PSK").unwrap(),
            release_keys: vec![],
            block_when_fullscreen: false,
            notify: false,
            switch_hint: None,
            ignore_devices: vec![],
            peers: vec![],
            view: Default::default(),
        }
        .psk_bytes()
        .unwrap();
        // The daemon has us on its left, so we enter its left edge and leave through it.
        let side: Side = match std::env::var("EDGEHOP_PROBE_SIDE").as_deref() {
            Ok("right") => Side::Right,
            _ => Side::Left,
        };
        let s = TcpStream::connect(addr).await.unwrap();
        let (mut r, mut w) = proto::handshake(s, &psk, true).await.unwrap();
        w.send(&Msg::Hello {
            name: "probe".into(),
            version: PROTOCOL_VERSION,
            video: None,
        })
        .await
        .unwrap();
        assert!(matches!(r.recv().await.unwrap(), Msg::Hello { .. }));
        let desc = DeviceDesc {
            name: "Probe mouse".into(),
            bus: 3,
            vendor: 0x1234,
            product: 0x5678,
            version: 1,
            props: vec![],
            keys: vec![0x110, 0x111, 0x112],
            rel: vec![0, 1, 8],
            abs: vec![],
            msc: vec![],
            pointer: true,
            touchpad: false,
            keyboard: false,
        };
        w.send(&Msg::AddDevice { id: 77, desc }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        w.send(&Msg::Enter { side, frac: 0.5 }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Move inward a bit, then back out through the edge we came in by.
        let dir = if side == Side::Left { 1 } else { -1 };
        for _ in 0..20 {
            w.send(&Msg::Input {
                id: 77,
                events: vec![Ev {
                    t: 2,
                    c: 0,
                    v: 10 * dir,
                }],
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let back = async {
            loop {
                if let Msg::Released { frac } = r.recv().await.unwrap() {
                    return frac;
                }
            }
        };
        let pump = async {
            for _ in 0..400 {
                w.send(&Msg::Input {
                    id: 77,
                    events: vec![Ev {
                        t: 2,
                        c: 0,
                        v: -10 * dir,
                    }],
                })
                .await
                .unwrap();
                tokio::time::sleep(Duration::from_millis(4)).await;
            }
        };
        tokio::select! {
            frac = back => assert!((frac - 0.5).abs() < 0.05, "came back at {frac}"),
            _ = pump => panic!("never handed back"),
        }
    }

    /// Like the probe above, but a one-finger touchpad swipe through a copy
    /// of a real touchpad (EDGEHOP_PROBE_DESC = JSON DeviceDesc file).
    #[tokio::test]
    #[ignore]
    async fn probe_touchpad_return() {
        let addr = std::env::var("EDGEHOP_PROBE_ADDR").unwrap();
        let psk = crate::config::Config {
            name: String::new(),
            listen: None,
            psk: std::env::var("EDGEHOP_PROBE_PSK").unwrap(),
            release_keys: vec![],
            block_when_fullscreen: false,
            notify: false,
            switch_hint: None,
            ignore_devices: vec![],
            peers: vec![],
            view: Default::default(),
        }
        .psk_bytes()
        .unwrap();
        let desc: DeviceDesc = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("EDGEHOP_PROBE_DESC").unwrap()).unwrap(),
        )
        .unwrap();
        let ax = |c: u16| desc.abs.iter().find(|a| a.code == c).unwrap().clone();
        let (xa, ya) = (ax(0x00), ax(0x01));
        let s = TcpStream::connect(addr).await.unwrap();
        let (mut r, mut w) = proto::handshake(s, &psk, true).await.unwrap();
        w.send(&Msg::Hello {
            name: "probe".into(),
            version: PROTOCOL_VERSION,
            video: None,
        })
        .await
        .unwrap();
        assert!(matches!(r.recv().await.unwrap(), Msg::Hello { .. }));
        w.send(&Msg::AddDevice { id: 78, desc }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        w.send(&Msg::Enter {
            side: Side::Right,
            frac: 0.5,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let ev = |t: u16, c: u16, v: i32| Ev { t, c, v };
        let back = async {
            loop {
                if let Msg::Released { frac } = r.recv().await.unwrap() {
                    return frac;
                }
            }
        };
        let swipes = async {
            let y = (ya.min + ya.max) / 2;
            let mut tid = 100;
            let mut ts = 0i32;
            for swipe in 0..6 {
                tid += 1;
                // First swipe goes left (away from the edge), then right, repeatedly.
                let (from, to) = if swipe == 0 { (0.7, 0.3) } else { (0.2, 0.9) };
                let steps = 40;
                for i in 0..=steps {
                    let f = from + (to - from) * i as f64 / steps as f64;
                    let x = xa.min + ((xa.max - xa.min) as f64 * f) as i32;
                    let mut e = vec![ev(3, 0x2f, 0)];
                    if i == 0 {
                        e.push(ev(3, 0x39, tid));
                    }
                    e.push(ev(3, 0x35, x));
                    e.push(ev(3, 0x36, y));
                    if i == 0 {
                        e.push(ev(1, 0x14a, 1));
                        e.push(ev(1, 0x145, 1));
                    }
                    e.push(ev(3, 0x00, x));
                    e.push(ev(3, 0x01, y));
                    ts += 8000;
                    e.push(ev(4, 5, ts)); // MSC_TIMESTAMP
                    w.send(&Msg::Input { id: 78, events: e }).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(8)).await;
                }
                let lift = vec![
                    ev(3, 0x2f, 0),
                    ev(3, 0x39, -1),
                    ev(1, 0x14a, 0),
                    ev(1, 0x145, 0),
                ];
                w.send(&Msg::Input {
                    id: 78,
                    events: lift,
                })
                .await
                .unwrap();
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
        };
        tokio::select! {
            frac = back => println!("handed back at {frac}"),
            _ = swipes => panic!("never handed back"),
        }
    }
}
