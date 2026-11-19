use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::core::{PCWSTR, Result, implement};

const SETTLE: Duration = Duration::from_secs(3);
const RETRY: Duration = Duration::from_secs(6);
const MIN_PEAK: f32 = 0.001;

enum Change {
    DefaultOutput,
    Device(String),
}

#[implement(IMMNotificationClient)]
struct Notifications(Sender<Change>);

impl Notifications {
    fn device(&self, id: &PCWSTR) {
        if !id.is_null()
            && let Ok(id) = unsafe { id.to_string() }
        {
            let _ = self.0.send(Change::Device(id));
        }
    }
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for Notifications_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, _: DEVICE_STATE) -> Result<()> {
        self.device(id);
        Ok(())
    }
    fn OnDeviceAdded(&self, id: &PCWSTR) -> Result<()> {
        self.device(id);
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> Result<()> {
        self.device(id);
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> Result<()> {
        if flow == eRender && role == eMultimedia {
            let _ = self.0.send(Change::DefaultOutput);
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

fn device_id(device: &IMMDevice) -> Result<String> {
    unsafe {
        let id = device.GetId()?;
        let value = PCWSTR(id.0).to_string();
        CoTaskMemFree(Some(id.0.cast()));
        Ok(value?)
    }
}

fn audible(device: &IMMDevice) -> Result<bool> {
    unsafe {
        let meter: IAudioMeterInformation = device.Activate(CLSCTX_ALL, None)?;
        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
        Ok(!volume.GetMute()?.as_bool()
            && volume.GetMasterVolumeLevelScalar()? > 0.0
            && meter.GetPeakValue()? >= MIN_PEAK)
    }
}

#[derive(Default)]
struct Recovery {
    initialized: bool,
    output: Option<String>,
    due: Option<Instant>,
    last_attempt: Option<Instant>,
    attempts: u8,
    heard: bool,
}

impl Recovery {
    fn observe(
        &mut self,
        now: Instant,
        output: Option<String>,
        changes: impl Iterator<Item = Change>,
    ) {
        let mut changed = self.output != output;
        for change in changes {
            changed |= match change {
                Change::DefaultOutput => true,
                Change::Device(id) => {
                    self.output.as_ref() == Some(&id) || output.as_ref() == Some(&id)
                }
            };
        }
        self.output = output;
        if self.initialized && changed {
            self.attempts = 0;
            self.heard = false;
            self.due = self.output.as_ref().map(|_| {
                let stable = now + SETTLE;
                self.last_attempt
                    .map_or(stable, |last| stable.max(last + RETRY))
            });
        }
        self.initialized = true;
    }

    fn ready(&mut self, now: Instant, audible: bool) -> bool {
        if self.due.is_none_or(|due| now < due) {
            return false;
        }
        let sustained = self.heard && audible;
        self.heard = audible;
        if sustained {
            self.last_attempt = Some(now);
            self.due = None;
            self.heard = false;
            self.attempts += 1;
        }
        sustained
    }

    fn retry(&mut self, now: Instant) {
        if self.attempts == 1 {
            self.due = Some(now + RETRY);
        }
    }
}

pub struct OutputWatch {
    devices: IMMDeviceEnumerator,
    callback: Option<IMMNotificationClient>,
    changes: Receiver<Change>,
    recovery: Recovery,
}

impl OutputWatch {
    pub fn open() -> Result<Self> {
        let devices: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let (send, changes) = mpsc::channel();
        let callback: IMMNotificationClient = Notifications(send).into();
        let registered = unsafe { devices.RegisterEndpointNotificationCallback(&callback) }.is_ok();
        Ok(Self {
            devices,
            callback: registered.then_some(callback),
            changes,
            recovery: Recovery::default(),
        })
    }

    fn endpoint(&self) -> Result<Option<(String, IMMDevice)>> {
        match unsafe { self.devices.GetDefaultAudioEndpoint(eRender, eMultimedia) } {
            Ok(device) => Ok(Some((device_id(&device)?, device))),
            Err(err) if err.code().0 == 0x80070490u32 as i32 => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn status(&self) -> Result<(bool, Option<bool>)> {
        let sound = self
            .endpoint()?
            .map(|(_, device)| audible(&device))
            .transpose()?;
        Ok((self.callback.is_some(), sound))
    }

    pub fn ready(&mut self, now: Instant) -> Result<bool> {
        let endpoint = self.endpoint()?;
        let (id, device) = endpoint.unzip();
        self.recovery.observe(now, id, self.changes.try_iter());
        let Some(device) = device.filter(|_| self.recovery.due.is_some_and(|due| now >= due))
        else {
            return Ok(false);
        };
        Ok(self.recovery.ready(now, audible(&device).unwrap_or(false)))
    }

    pub fn retry(&mut self, now: Instant) {
        self.recovery.retry(now);
    }
}

impl Drop for OutputWatch {
    fn drop(&mut self) {
        if let Some(callback) = &self.callback {
            let _ = unsafe {
                self.devices
                    .UnregisterEndpointNotificationCallback(callback)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_reconnect_on_the_same_endpoint_waits_for_real_sound_and_fires_once() {
        let t = Instant::now();
        let mut state = Recovery::default();
        let headphones = || Some(String::from("headphones"));
        state.observe(t, headphones(), [Change::DefaultOutput].into_iter());
        assert!(!state.ready(t + SETTLE, true));
        state.observe(
            t,
            headphones(),
            [Change::Device("microphone".into())].into_iter(),
        );
        assert!(state.due.is_none());
        state.observe(
            t,
            headphones(),
            [Change::Device("headphones".into()), Change::DefaultOutput].into_iter(),
        );
        assert!(!state.ready(t, true));
        let settled = t + SETTLE;
        assert!(!state.ready(settled, false));
        assert!(!state.ready(settled, true));
        assert!(!state.ready(settled, false));
        assert!(!state.ready(settled, true));
        assert!(state.ready(settled, true));
        assert!(!state.ready(settled + RETRY, true));
    }

    #[test]
    fn missing_device_polling_and_failures_do_not_loop() {
        let t = Instant::now();
        let mut state = Recovery::default();
        state.observe(t, Some("A".into()), std::iter::empty());
        state.observe(t, None, std::iter::empty());
        assert!(state.due.is_none());
        state.observe(t, Some("A".into()), std::iter::empty());
        state.observe(t + SETTLE, Some("B".into()), std::iter::empty());
        assert!(!state.ready(t + SETTLE, true));
        let first = t + SETTLE + SETTLE;
        assert!(!state.ready(first, true));
        assert!(state.ready(first, true));
        state.retry(first);
        assert!(!state.ready(first, true));
        let second = first + RETRY;
        assert!(!state.ready(second, true));
        assert!(state.ready(second, true));
        state.retry(second);
        assert!(state.due.is_none());
    }
}
