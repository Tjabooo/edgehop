//! Local input devices: find keyboards, mice and touchpads, read them
//! passively, and grab them (EVIOCGRAB) while another machine is controlled.

use std::collections::{BTreeSet, HashMap};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use evdev::{AbsoluteAxisCode as Abs, EventType, KeyCode, PropType, RelativeAxisCode as Rel};
use tokio::sync::mpsc;

use crate::proto::{AbsAxis, DeviceDesc, Ev};

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const EV_ABS: u16 = 3;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;

/// Keys never forwarded: they would suspend or power off the other machine.
const BLOCKED_KEYS: &[u16] = &[
    116, // KEY_POWER
    142, // KEY_SLEEP
    143, // KEY_WAKEUP
    205, // KEY_SUSPEND
    356, // KEY_POWER2
];

/// Touch "tools" are not held buttons for the purpose of "is anything pressed".
fn is_touch_tool(code: u16) -> bool {
    (0x140..=0x14f).contains(&code) // BTN_TOOL_* , BTN_TOUCH, BTN_STYLUS*, …
}

pub const MIRROR_PREFIX: &str = "edgehop ";

/// Everything the device manager tells the daemon.
pub enum DevEvent {
    Added {
        id: u32,
        desc: DeviceDesc,
        grab_fd: OwnedFd,
        path: PathBuf,
    },
    Batch {
        id: u32,
        events: Vec<Ev>,
    },
    Removed {
        id: u32,
    },
}

/// What a device looked like (for `edgehop check`).
pub struct Probe {
    pub path: PathBuf,
    pub name: String,
    pub verdict: Result<DeviceDesc, &'static str>,
}

fn is_virtual(path: &Path) -> bool {
    let Some(file) = path.file_name() else {
        return false;
    };
    let sys = Path::new("/sys/class/input").join(file);
    std::fs::canonicalize(sys)
        .map(|p| p.to_string_lossy().contains("/devices/virtual/"))
        .unwrap_or(false)
}

/// Decide whether a device is captured, and describe it if so.
pub fn describe(
    dev: &evdev::raw_stream::RawDevice,
    path: &Path,
    ignore: &[String],
) -> Result<DeviceDesc, &'static str> {
    let name = dev.name().unwrap_or("").to_string();
    if name.starts_with(MIRROR_PREFIX.trim_end()) {
        return Err("edgehop's own virtual device");
    }
    if is_virtual(path) {
        return Err("virtual device (uinput)");
    }
    let lname = name.to_lowercase();
    if ignore
        .iter()
        .any(|i| !i.is_empty() && lname.contains(&i.to_lowercase()))
    {
        return Err("listed in ignore_devices");
    }
    let props = dev.properties();
    let keys = dev.supported_keys();
    let has_key = |k: KeyCode| keys.is_some_and(|s| s.contains(k));
    let rel = dev.supported_relative_axes();
    let abs = dev.supported_absolute_axes();

    let keyboard = [
        KeyCode::KEY_A,
        KeyCode::KEY_Z,
        KeyCode::KEY_SPACE,
        KeyCode::KEY_ENTER,
    ]
    .into_iter()
    .all(has_key);
    let pointer = rel.is_some_and(|r| r.contains(Rel::REL_X) && r.contains(Rel::REL_Y));
    let touchpad = abs
        .is_some_and(|a| a.contains(Abs::ABS_MT_POSITION_X) && a.contains(Abs::ABS_X))
        && (has_key(KeyCode::BTN_TOUCH) || has_key(KeyCode::BTN_TOOL_FINGER))
        && !props.contains(PropType::DIRECT);
    if !(keyboard || pointer || touchpad) {
        return Err("not a keyboard, mouse or touchpad");
    }

    let abs_state = dev.get_abs_state().map_err(|_| "could not read axes")?;
    let mut abs_axes = Vec::new();
    if let Some(a) = abs {
        for code in a.iter() {
            let i = code.0 as usize;
            let s = abs_state.get(i).ok_or("bad axis")?;
            abs_axes.push(AbsAxis {
                code: code.0,
                min: s.minimum,
                max: s.maximum,
                fuzz: s.fuzz,
                flat: s.flat,
                res: s.resolution,
            });
        }
    }
    let id = dev.input_id();
    Ok(DeviceDesc {
        name,
        bus: id.bus_type().0,
        vendor: id.vendor(),
        product: id.product(),
        version: id.version(),
        props: props.iter().map(|p| p.0).collect(),
        keys: keys
            .map(|k| {
                k.iter()
                    .map(|c| c.code())
                    .filter(|c| !BLOCKED_KEYS.contains(c))
                    .collect()
            })
            .unwrap_or_default(),
        rel: rel
            .map(|r| r.iter().map(|c| c.0).collect())
            .unwrap_or_default(),
        abs: abs_axes,
        msc: if dev.supported_events().contains(EventType::MISC) {
            dev.misc_properties()
                .map(|m| m.iter().map(|c| c.0).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        },
        pointer,
        touchpad,
        keyboard,
    })
}

pub fn probe_all(ignore: &[String]) -> Vec<Probe> {
    let mut out: Vec<Probe> = evdev::raw_stream::enumerate()
        .map(|(path, dev)| Probe {
            name: dev.name().unwrap_or("?").to_string(),
            verdict: describe(&dev, &path, ignore),
            path,
        })
        .collect();
    out.sort_by_key(|p| event_number(&p.path));
    out
}

fn event_number(p: &Path) -> u32 {
    p.file_name()
        .and_then(|f| f.to_str())
        .and_then(|f| f.strip_prefix("event"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Touch "tool" keys libinput uses to count fingers on a touchpad.
const TOUCH_TOOL_KEYS: &[u16] = &[
    0x14a, // BTN_TOUCH
    0x145, // BTN_TOOL_FINGER
    0x14d, // BTN_TOOL_DOUBLETAP
    0x14e, // BTN_TOOL_TRIPLETAP
    0x14f, // BTN_TOOL_QUADTAP
    0x148, // BTN_TOOL_QUINTTAP
];

/// One frame that walks every touch tool key the device has through the
/// opposite value and back to its real one. The kernel drops "no change"
/// events, so this is the only way to make other readers see the true state.
pub fn touch_resync_frame(caps: &[u16], held: impl Fn(u16) -> bool) -> Vec<Ev> {
    let mut out = Vec::new();
    for &c in TOUCH_TOOL_KEYS.iter().filter(|c| caps.contains(c)) {
        let now = held(c) as i32;
        out.push(Ev {
            t: EV_KEY,
            c,
            v: 1 - now,
        });
        out.push(Ev {
            t: EV_KEY,
            c,
            v: now,
        });
    }
    if !out.is_empty() {
        out.push(Ev {
            t: EV_SYN,
            c: SYN_REPORT,
            v: 0,
        });
    }
    out
}

/// After a grab ends, the compositor's libinput still holds the touchpad
/// state from when the grab began: it never saw the finger-count changes in
/// between, and ends up with an impossible state ("Invalid fake finger
/// state") that breaks multi-finger gestures. Re-send the real state.
pub fn resync_touchpad(path: &Path, fd: &OwnedFd, caps: &[u16]) -> std::io::Result<()> {
    // EVIOCGKEY(len) = _IOR('E', 0x18, len)
    let mut bits = [0u8; 96];
    let req: libc::c_ulong = (2 << 30) | ((bits.len() as libc::c_ulong) << 16) | (0x45 << 8) | 0x18;
    // SAFETY: the kernel writes at most bits.len() bytes into our buffer.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, bits.as_mut_ptr()) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let held = |c: u16| {
        bits.get(c as usize / 8)
            .is_some_and(|b| b >> (c % 8) & 1 == 1)
    };
    let frame = touch_resync_frame(caps, held);
    if frame.is_empty() {
        return Ok(());
    }
    let mut raw = Vec::with_capacity(frame.len() * std::mem::size_of::<libc::input_event>());
    for e in frame {
        let ie = libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: e.t,
            code: e.c,
            value: e.v,
        };
        // SAFETY: input_event is plain old data.
        raw.extend_from_slice(unsafe {
            std::slice::from_raw_parts(&ie as *const _ as *const u8, std::mem::size_of_val(&ie))
        });
    }
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(&raw)
}

/// EVIOCGRAB = _IOW('E', 0x90, int)
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

pub fn set_grab(fd: &OwnedFd, grab: bool) -> std::io::Result<()> {
    let arg: libc::c_int = grab as libc::c_int;
    // SAFETY: plain ioctl on an evdev fd we own; arg is passed by value.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCGRAB as _, arg as libc::c_ulong) };
    if r < 0 {
        let e = std::io::Error::last_os_error();
        // EINVAL on ungrab = was not grabbed; EBUSY on grab = someone else holds it.
        if !grab && e.raw_os_error() == Some(libc::EINVAL) {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

/// Scan /dev/input every couple of seconds and start a reader per new device.
pub async fn manager(ignore: Vec<String>, tx: mpsc::Sender<DevEvent>) {
    let mut known: HashMap<PathBuf, u32> = HashMap::new();
    let mut skipped: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut next_id: u32 = 1;
    let (gone_tx, mut gone_rx) = mpsc::unbounded_channel::<PathBuf>();
    loop {
        while let Ok(p) = gone_rx.try_recv() {
            known.remove(&p);
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir("/dev/input")
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|f| f.to_str())
                            .is_some_and(|f| f.starts_with("event"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by_key(|p| event_number(p));
        for path in entries {
            if known.contains_key(&path) {
                continue;
            }
            let sys = std::fs::canonicalize(
                Path::new("/sys/class/input").join(path.file_name().unwrap()),
            )
            .unwrap_or_default();
            if skipped.get(&path) == Some(&sys) {
                continue;
            }
            let dev = match evdev::raw_stream::RawDevice::open(&path) {
                Ok(d) => d,
                Err(e) => {
                    log::debug!("open {}: {e}", path.display());
                    skipped.insert(path, sys);
                    continue;
                }
            };
            let desc = match describe(&dev, &path, &ignore) {
                Ok(d) => d,
                Err(why) => {
                    log::debug!(
                        "skip {} ({}): {why}",
                        path.display(),
                        dev.name().unwrap_or("?")
                    );
                    skipped.insert(path, sys);
                    continue;
                }
            };
            skipped.remove(&path);
            let grab_fd = match dev.as_raw_fd() {
                fd if fd >= 0 => match nix_dup(fd) {
                    Ok(f) => f,
                    Err(e) => {
                        log::warn!("dup {}: {e}", path.display());
                        continue;
                    }
                },
                _ => continue,
            };
            let stream = match dev.into_event_stream() {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("stream {}: {e}", path.display());
                    continue;
                }
            };
            let id = next_id;
            next_id += 1;
            log::info!("input device {id}: {} ({})", desc.name, path.display());
            known.insert(path.clone(), id);
            if tx
                .send(DevEvent::Added {
                    id,
                    desc,
                    grab_fd,
                    path: path.clone(),
                })
                .await
                .is_err()
            {
                return;
            }
            tokio::spawn(reader(id, path, stream, tx.clone(), gone_tx.clone()));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn nix_dup(fd: i32) -> std::io::Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: F_DUPFD_CLOEXEC returns a new fd we then own.
    let n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

async fn reader(
    id: u32,
    path: PathBuf,
    mut stream: evdev::raw_stream::EventStream,
    tx: mpsc::Sender<DevEvent>,
    gone: mpsc::UnboundedSender<PathBuf>,
) {
    let mut batch: Vec<Ev> = Vec::with_capacity(16);
    let mut dropping = false;
    loop {
        match stream.next_event().await {
            Ok(e) => {
                let (t, c, v) = (e.event_type().0, e.code(), e.value());
                if t == EV_SYN {
                    match c {
                        SYN_REPORT => {
                            if dropping {
                                dropping = false;
                                batch.clear();
                            } else if !batch.is_empty() {
                                let events = std::mem::replace(&mut batch, Vec::with_capacity(16));
                                if tx.send(DevEvent::Batch { id, events }).await.is_err() {
                                    return;
                                }
                            }
                        }
                        SYN_DROPPED => {
                            dropping = true;
                            batch.clear();
                        }
                        _ => {}
                    }
                } else if !(t == EV_KEY && BLOCKED_KEYS.contains(&c)) {
                    batch.push(Ev { t, c, v });
                }
            }
            Err(e) => {
                log::info!("input device {id} gone ({}): {e}", path.display());
                let _ = gone.send(path);
                let _ = tx.send(DevEvent::Removed { id }).await;
                return;
            }
        }
    }
}

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const BTN_TOUCH: u16 = 0x14a;
const BTN_TOOL_FINGER: u16 = 0x145;

#[derive(Default, Clone, Copy)]
struct Slot {
    tracking: i32,
    x: i32,
    y: i32,
}

/// Tracks held keys and touches of one device, from its event stream.
/// Used on both ends: to know whether anything is held, to work out which
/// way the pointer is moving, and to cleanly lift everything on handover.
#[derive(Default)]
pub struct InputState {
    pub keys: BTreeSet<u16>,
    slot: usize,
    slots: Vec<Slot>,
    abs_x: Option<i32>,
    abs_y: Option<i32>,
    /// Single-finger position at the end of the previous report.
    last_touch: Option<(i32, i32)>,
}

impl InputState {
    pub fn new() -> InputState {
        InputState::default()
    }

    /// Apply one report; returns the pointer motion it represents
    /// (only the sign matters: rel counts for mice, abs units for touchpads).
    pub fn apply(&mut self, events: &[Ev]) -> (f64, f64) {
        let (mut dx, mut dy) = (0.0, 0.0);
        let mut touch_restart = false;
        for e in events {
            match e.t {
                EV_KEY => {
                    if e.v == 0 {
                        self.keys.remove(&e.c);
                    } else if e.v == 1 {
                        self.keys.insert(e.c);
                    }
                    if e.c == BTN_TOUCH || e.c == BTN_TOOL_FINGER {
                        touch_restart = true;
                    }
                }
                EV_REL => match e.c {
                    0 => dx += e.v as f64,
                    1 => dy += e.v as f64,
                    _ => {}
                },
                EV_ABS => match e.c {
                    ABS_X => self.abs_x = Some(e.v),
                    ABS_Y => self.abs_y = Some(e.v),
                    ABS_MT_SLOT => self.slot = e.v.max(0) as usize,
                    ABS_MT_TRACKING_ID | ABS_MT_POSITION_X | ABS_MT_POSITION_Y => {
                        if self.slot >= self.slots.len() {
                            if self.slot > 64 {
                                continue;
                            }
                            self.slots.resize(
                                self.slot + 1,
                                Slot {
                                    tracking: -1,
                                    x: 0,
                                    y: 0,
                                },
                            );
                        }
                        let s = &mut self.slots[self.slot];
                        match e.c {
                            ABS_MT_TRACKING_ID => s.tracking = e.v,
                            ABS_MT_POSITION_X => s.x = e.v,
                            _ => s.y = e.v,
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        // Touchpad: one finger down, moving → motion from the ABS_X/Y deltas.
        let one_finger = self.keys.contains(&BTN_TOOL_FINGER) && self.keys.contains(&BTN_TOUCH);
        match (one_finger, self.abs_x, self.abs_y) {
            (true, Some(x), Some(y)) => {
                if let (Some((lx, ly)), false) = (self.last_touch, touch_restart) {
                    dx += (x - lx) as f64;
                    dy += (y - ly) as f64;
                }
                self.last_touch = Some((x, y));
            }
            _ => self.last_touch = None,
        }
        (dx, dy)
    }

    /// Any key or button held (touches don't count)?
    pub fn anything_held(&self) -> bool {
        self.keys.iter().any(|k| !is_touch_tool(*k))
    }

    /// Events that recreate the current touches on a fresh device, so a
    /// swipe that crosses over keeps going on the other side.
    pub fn touch_resume(&self) -> Vec<Ev> {
        let mut out = Vec::new();
        let active: Vec<(usize, &Slot)> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.tracking >= 0)
            .collect();
        if active.is_empty() {
            return out;
        }
        for (i, s) in &active {
            out.push(Ev {
                t: EV_ABS,
                c: ABS_MT_SLOT,
                v: *i as i32,
            });
            out.push(Ev {
                t: EV_ABS,
                c: ABS_MT_TRACKING_ID,
                v: s.tracking,
            });
            out.push(Ev {
                t: EV_ABS,
                c: ABS_MT_POSITION_X,
                v: s.x,
            });
            out.push(Ev {
                t: EV_ABS,
                c: ABS_MT_POSITION_Y,
                v: s.y,
            });
        }
        out.push(Ev {
            t: EV_ABS,
            c: ABS_MT_SLOT,
            v: self.slot as i32,
        });
        for k in &self.keys {
            if is_touch_tool(*k) {
                out.push(Ev {
                    t: EV_KEY,
                    c: *k,
                    v: 1,
                });
            }
        }
        if let (Some(x), Some(y)) = (self.abs_x, self.abs_y) {
            out.push(Ev {
                t: EV_ABS,
                c: ABS_X,
                v: x,
            });
            out.push(Ev {
                t: EV_ABS,
                c: ABS_Y,
                v: y,
            });
        }
        out
    }

    /// Events that lift every held key and touch; resets the state.
    pub fn release_all(&mut self) -> Vec<Ev> {
        let mut out = Vec::new();
        for (i, s) in self.slots.iter_mut().enumerate() {
            if s.tracking >= 0 {
                out.push(Ev {
                    t: EV_ABS,
                    c: ABS_MT_SLOT,
                    v: i as i32,
                });
                out.push(Ev {
                    t: EV_ABS,
                    c: ABS_MT_TRACKING_ID,
                    v: -1,
                });
                s.tracking = -1;
            }
        }
        for k in std::mem::take(&mut self.keys) {
            out.push(Ev {
                t: EV_KEY,
                c: k,
                v: 0,
            });
        }
        self.last_touch = None;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EDGEHOP_MIRROR_TEST=/dev/input/eventN cargo test dump_desc -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_desc() {
        let path = std::env::var("EDGEHOP_MIRROR_TEST").unwrap();
        let dev = evdev::raw_stream::RawDevice::open(&path).unwrap();
        let d = describe(&dev, Path::new(&path), &[]).unwrap();
        println!("DESC {}", serde_json::to_string(&d).unwrap());
    }

    fn k(c: u16, v: i32) -> Ev {
        Ev { t: EV_KEY, c, v }
    }
    fn a(c: u16, v: i32) -> Ev {
        Ev { t: EV_ABS, c, v }
    }

    #[test]
    fn resync_frame_ends_on_real_state() {
        let caps = [0x110, 0x14a, 0x145, 0x14d];
        let f = touch_resync_frame(&caps, |c| c == 0x145);
        assert_eq!(f.len(), 7);
        assert_eq!(f[2], k(0x145, 0));
        assert_eq!(f[3], k(0x145, 1));
        assert_eq!(f[5], k(0x14d, 0));
        assert!(touch_resync_frame(&[0x110], |_| false).is_empty());
    }

    #[test]
    fn mouse_motion_and_held_buttons() {
        let mut s = InputState::new();
        assert_eq!(
            s.apply(&[
                Ev {
                    t: EV_REL,
                    c: 0,
                    v: -4
                },
                Ev {
                    t: EV_REL,
                    c: 1,
                    v: 2
                }
            ]),
            (-4.0, 2.0)
        );
        s.apply(&[k(0x110, 1)]); // BTN_LEFT
        assert!(s.anything_held());
        s.apply(&[k(0x110, 0)]);
        assert!(!s.anything_held());
    }

    #[test]
    fn touchpad_motion_and_resume() {
        let mut s = InputState::new();
        let down = [
            a(ABS_MT_SLOT, 0),
            a(ABS_MT_TRACKING_ID, 42),
            a(ABS_MT_POSITION_X, 500),
            a(ABS_MT_POSITION_Y, 300),
            k(BTN_TOUCH, 1),
            k(BTN_TOOL_FINGER, 1),
            a(ABS_X, 500),
            a(ABS_Y, 300),
        ];
        assert_eq!(s.apply(&down), (0.0, 0.0));
        assert!(
            !s.anything_held(),
            "a finger on the pad is not a held button"
        );
        let (dx, _) = s.apply(&[a(ABS_MT_POSITION_X, 480), a(ABS_X, 480)]);
        assert_eq!(dx, -20.0);
        let resume = s.touch_resume();
        assert!(resume.contains(&a(ABS_MT_TRACKING_ID, 42)));
        assert!(resume.contains(&a(ABS_MT_POSITION_X, 480)));
        assert!(resume.contains(&k(BTN_TOUCH, 1)));
        let lift = s.release_all();
        assert!(lift.contains(&a(ABS_MT_TRACKING_ID, -1)));
        assert!(lift.contains(&k(BTN_TOOL_FINGER, 0)));
        assert!(s.touch_resume().is_empty());
    }
}
