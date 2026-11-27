//! Receiver side: a uinput copy of each of the controller's devices, so the
//! compositor (libinput) treats them like the real thing: touchpad gestures,
//! tap-to-click, acceleration and keyboard layout all happen locally.

use std::collections::HashMap;

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventSummary, FFEffectCode, FFEffectKind,
    InputEvent, InputId, KeyCode, MiscCode, PropType, RelativeAxisCode, UInputCode, UinputAbsSetup,
};
use tokio::sync::mpsc;

use crate::devices::{self, InputState, MIRROR_PREFIX};
use crate::proto::{DeviceDesc, Ev};

/// Called with (strong, weak, ms) whenever a game starts or stops rumbling
/// a gamepad mirror; (0, 0, 0) = stop.
pub type RumbleSink = Box<dyn FnMut(u16, u16, u16) + Send>;

enum Out {
    Direct(VirtualDevice),
    /// Rumble-capable gamepads: a task owns the device, writes our reports
    /// and answers the kernel's force-feedback requests.
    Task(mpsc::UnboundedSender<Vec<InputEvent>>),
}

pub struct Mirror {
    out: Out,
    pub desc: DeviceDesc,
    pub state: InputState,
    keys: AttributeSet<KeyCode>,
    buf: Vec<InputEvent>,
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl Mirror {
    /// `rumble`: where a rumble-capable gamepad's effects go (else none is offered).
    pub fn create(desc: DeviceDesc, rumble: Option<RumbleSink>) -> Result<Mirror> {
        let name = format!("{MIRROR_PREFIX}{}", truncate(&desc.name, 70));
        let id = InputId::new(BusType(desc.bus), desc.vendor, desc.product, desc.version);
        let keys: AttributeSet<KeyCode> = desc.keys.iter().map(|k| KeyCode::new(*k)).collect();
        let mut b = VirtualDevice::builder()
            .context("open /dev/uinput (is this user in the input group?)")?
            .name(name.as_bytes())
            .input_id(id);
        if !desc.keys.is_empty() {
            b = b.with_keys(&keys)?;
        }
        if !desc.rel.is_empty() {
            let rel: AttributeSet<RelativeAxisCode> =
                desc.rel.iter().map(|r| RelativeAxisCode(*r)).collect();
            b = b.with_relative_axes(&rel)?;
        }
        for a in &desc.abs {
            let info = AbsInfo::new(0, a.min, a.max, a.fuzz, a.flat, a.res);
            b = b.with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode(a.code), info))?;
        }
        if !desc.props.is_empty() {
            let props: AttributeSet<PropType> = desc.props.iter().map(|p| PropType(*p)).collect();
            b = b.with_properties(&props)?;
        }
        if !desc.msc.is_empty() {
            let msc: AttributeSet<MiscCode> = desc.msc.iter().map(|m| MiscCode(*m)).collect();
            b = b.with_msc(&msc)?;
        }
        let rumble = rumble.filter(|_| desc.gamepad && desc.rumble);
        if rumble.is_some() {
            let ff: AttributeSet<FFEffectCode> = [FFEffectCode::FF_RUMBLE].into_iter().collect();
            b = b.with_ff(&ff)?.with_ff_effects_max(FF_EFFECTS_MAX);
        }
        let dev = b.build().context("create uinput device")?;
        let out = match rumble {
            Some(sink) => {
                let stream = dev.into_event_stream().context("watch uinput for rumble")?;
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(rumble_task(stream, rx, sink));
                Out::Task(tx)
            }
            None => Out::Direct(dev),
        };
        Ok(Mirror {
            out,
            desc,
            state: InputState::new(),
            keys,
            buf: Vec::new(),
        })
    }

    /// Replay one report. Returns its pointer motion (see InputState::apply).
    pub fn emit(&mut self, events: &[Ev]) -> Result<(f64, f64)> {
        let motion = self.state.apply(events);
        self.write(events)?;
        Ok(motion)
    }

    fn write(&mut self, events: &[Ev]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        self.buf.clear();
        self.buf.extend(
            events
                .iter()
                // Keys the device doesn't declare would be dropped by the kernel anyway.
                .filter(|e| e.t != 1 || self.keys.contains(KeyCode::new(e.c)))
                .map(|e| InputEvent::new(e.t, e.c, e.v)),
        );
        match &mut self.out {
            Out::Direct(dev) => dev.emit(&self.buf)?,
            Out::Task(tx) => tx
                .send(self.buf.clone())
                .map_err(|_| anyhow::anyhow!("rumble task ended"))?,
        }
        Ok(())
    }

    /// Lift every held key, button and touch; gamepads also let go of their
    /// sticks and triggers.
    pub fn release_all(&mut self) {
        let mut lift = self.state.release_all();
        if self.desc.gamepad {
            lift.extend(self.desc.abs.iter().map(|a| Ev {
                t: devices::EV_ABS,
                c: a.code,
                v: devices::axis_rest(a, &self.desc.abs),
            }));
        }
        if let Err(e) = self.write(&lift) {
            log::warn!("release on {}: {e}", self.desc.name);
        }
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.release_all();
    }
}

const FF_EFFECTS_MAX: u32 = 16;

/// Owns a rumble-capable mirror: writes the reports, stores the effects
/// games upload and reports each play/stop to `sink`.
async fn rumble_task(
    mut stream: evdev::uinput::VirtualEventStream,
    mut rx: mpsc::UnboundedReceiver<Vec<InputEvent>>,
    mut sink: RumbleSink,
) {
    // effect id → (strong, weak, ms)
    let mut effects: HashMap<i16, (u16, u16, u16)> = HashMap::new();
    let mut playing: Option<i16> = None;
    let mut ff_ok = true;
    loop {
        tokio::select! {
            batch = rx.recv() => match batch {
                Some(b) => {
                    if let Err(e) = stream.device_mut().emit(&b) {
                        log::warn!("inject: {e}");
                    }
                }
                None => break,
            },
            ev = stream.next_event(), if ff_ok => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(e) => {
                        log::warn!("rumble: {e}");
                        ff_ok = false;
                        continue;
                    }
                };
                match ev.destructure() {
                    EventSummary::UInput(u, UInputCode::UI_FF_UPLOAD, _) => {
                        let mut up = match stream.device_mut().process_ff_upload(u) {
                            Ok(up) => up,
                            Err(e) => {
                                log::warn!("rumble upload: {e}");
                                continue;
                            }
                        };
                        let data = up.effect();
                        let FFEffectKind::Rumble { strong_magnitude, weak_magnitude } = data.kind
                        else {
                            up.set_retval(-libc::EINVAL);
                            continue;
                        };
                        let mut id = up.effect_id();
                        if id < 0 {
                            match (0..FF_EFFECTS_MAX as i16).find(|i| !effects.contains_key(i)) {
                                Some(free) => id = free,
                                None => {
                                    up.set_retval(-libc::ENOSPC);
                                    continue;
                                }
                            }
                            up.set_effect_id(id);
                        }
                        let fx = (strong_magnitude, weak_magnitude, data.replay.length);
                        effects.insert(id, fx);
                        up.set_retval(0);
                        // Games often change a rumble while it plays.
                        if playing == Some(id) {
                            sink(fx.0, fx.1, fx.2);
                        }
                    }
                    EventSummary::UInput(u, UInputCode::UI_FF_ERASE, _) => {
                        if let Ok(mut er) = stream.device_mut().process_ff_erase(u) {
                            let id = er.effect_id() as i16;
                            effects.remove(&id);
                            if playing == Some(id) {
                                playing = None;
                                sink(0, 0, 0);
                            }
                            er.set_retval(0);
                        }
                    }
                    EventSummary::ForceFeedback(_, code, value) => {
                        let id = code.0 as i16;
                        match (value > 0, effects.get(&id)) {
                            (true, Some(&(s, w, ms))) => {
                                playing = Some(id);
                                sink(s, w, ms);
                            }
                            _ if playing == Some(id) => {
                                playing = None;
                                sink(0, 0, 0);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if playing.is_some() {
        sink(0, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    /// Needs real hardware: `EDGEHOP_MIRROR_TEST=/dev/input/eventN cargo test -- --ignored`.
    /// Mirrors the device for a few seconds without sending any events.
    #[test]
    #[ignore]
    fn mirror_real_device() {
        let path = std::env::var("EDGEHOP_MIRROR_TEST").expect("set EDGEHOP_MIRROR_TEST");
        let dev = evdev::raw_stream::RawDevice::open(&path).unwrap();
        let desc = crate::devices::describe(&dev, std::path::Path::new(&path), &[]).unwrap();
        let _m = super::Mirror::create(desc, None).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
    }

    /// Needs /dev/uinput: `cargo test -- --ignored gamepad_mirror`.
    /// A fake rumble pad's mirror: buttons come out, and a game's rumble
    /// (upload, play, stop) reaches the sink.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn gamepad_mirror_buttons_and_rumble() {
        use crate::proto::{AbsAxis, DeviceDesc, Ev};
        use evdev::{FFEffectData, FFEffectKind, FFReplay, FFTrigger};
        let ax = |code, min, max| AbsAxis {
            code,
            min,
            max,
            fuzz: 16,
            flat: 128,
            res: 0,
        };
        let desc = DeviceDesc {
            name: "Test Pad".into(),
            bus: 3,
            vendor: 0x045e,
            product: 0x028e,
            version: 0x110,
            props: vec![],
            keys: vec![
                0x130, 0x131, 0x133, 0x134, 0x136, 0x137, 0x13a, 0x13b, 0x13c,
            ],
            rel: vec![],
            abs: vec![
                ax(0, -32768, 32767),
                ax(1, -32768, 32767),
                ax(2, 0, 255),
                ax(3, -32768, 32767),
                ax(4, -32768, 32767),
                ax(5, 0, 255),
            ],
            msc: vec![],
            pointer: false,
            touchpad: false,
            keyboard: false,
            gamepad: true,
            rumble: true,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink: super::RumbleSink = Box::new(move |s, w, ms| {
            let _ = tx.send((s, w, ms));
        });
        let mut m = super::Mirror::create(desc, Some(sink)).unwrap();
        // Find its node by name (udev needs a moment).
        let mut node = None;
        for _ in 0..50 {
            node = evdev::enumerate()
                .find(|(_, d)| d.name() == Some("edgehop Test Pad"))
                .map(|(p, _)| p);
            if node.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let node = node.expect("mirror node");
        let sys = std::path::Path::new("/sys/class/input").join(node.file_name().unwrap());
        eprintln!("mirror at {} ({})", node.display(), sys.display());
        let mut pad = evdev::Device::open(&node).unwrap();
        assert!(
            pad.supported_ff()
                .unwrap()
                .contains(evdev::FFEffectCode::FF_RUMBLE)
        );

        m.emit(&[
            Ev {
                t: 1,
                c: 0x130,
                v: 1,
            },
            Ev {
                t: 3,
                c: 0,
                v: 20000,
            },
        ])
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            pad.get_key_state()
                .unwrap()
                .contains(evdev::KeyCode::BTN_SOUTH)
        );
        assert_eq!(pad.get_abs_state().unwrap()[0].value, 20000);

        // A game rumbling: upload + play blocks on our task, so run it off-thread.
        let game = std::thread::spawn(move || {
            let mut fx = pad
                .upload_ff_effect(FFEffectData {
                    direction: 0,
                    trigger: FFTrigger::default(),
                    replay: FFReplay {
                        length: 300,
                        delay: 0,
                    },
                    kind: FFEffectKind::Rumble {
                        strong_magnitude: 40000,
                        weak_magnitude: 1000,
                    },
                })
                .unwrap();
            fx.play(1).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            fx.stop().unwrap();
            pad
        });
        let two_s = std::time::Duration::from_secs(2);
        let got = tokio::time::timeout(two_s, rx.recv())
            .await
            .expect("no rumble");
        assert_eq!(got, Some((40000, 1000, 300)));
        let got = tokio::time::timeout(two_s, rx.recv())
            .await
            .expect("no stop");
        assert_eq!(got, Some((0, 0, 0)));
        let pad = tokio::task::spawn_blocking(move || game.join().unwrap())
            .await
            .unwrap();

        // Letting go puts sticks back in the middle and lifts the button.
        m.release_all();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !pad.get_key_state()
                .unwrap()
                .contains(evdev::KeyCode::BTN_SOUTH)
        );
        assert_eq!(pad.get_abs_state().unwrap()[0].value, 0);
    }
}
