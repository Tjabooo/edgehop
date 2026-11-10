//! Receiver side: a uinput copy of each of the controller's devices, so the
//! compositor (libinput) treats them like the real thing: touchpad gestures,
//! tap-to-click, acceleration and keyboard layout all happen locally.

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, InputEvent, InputId, KeyCode, MiscCode,
    PropType, RelativeAxisCode, UinputAbsSetup,
};

use crate::devices::{InputState, MIRROR_PREFIX};
use crate::proto::{DeviceDesc, Ev};

pub struct Mirror {
    dev: VirtualDevice,
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
    pub fn create(desc: DeviceDesc) -> Result<Mirror> {
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
        let dev = b.build().context("create uinput device")?;
        Ok(Mirror {
            dev,
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
        self.dev.emit(&self.buf)?;
        Ok(())
    }

    /// Lift every held key, button and touch.
    pub fn release_all(&mut self) {
        let lift = self.state.release_all();
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
        let _m = super::Mirror::create(desc).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
