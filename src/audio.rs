use crate::{
    storage::{MuteMode, RestoreStore, VolumeBackup},
    volume::{self, VolumeEndpoint},
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::{
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{HWND, LPARAM, WPARAM},
        Media::Audio::{Endpoints::*, *},
        System::Com::{
            CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ,
            StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
        },
        UI::WindowsAndMessaging::PostMessageW,
    },
    core::{Interface, PCWSTR, PWSTR, implement},
};

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn take_string(pointer: PWSTR) -> Result<String> {
    let result = unsafe { pointer.to_string() };
    unsafe {
        CoTaskMemFree(Some(pointer.0.cast()));
    }
    Ok(result?)
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}

fn name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let properties = device.OpenPropertyStore(STGM_READ)?;
        let mut value = properties.GetValue(&PKEY_Device_FriendlyName)?;
        let result = PropVariantToStringAlloc(&value)
            .map_err(anyhow::Error::from)
            .and_then(take_string);
        PropVariantClear(&mut value)?;
        result
    }
}

#[derive(Clone, Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub available: bool,
}

pub fn devices() -> Result<Vec<Device>> {
    unsafe {
        let collection = enumerator()?.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        let mut result = Vec::new();
        for index in 0..collection.GetCount()? {
            let device = collection.Item(index)?;
            result.push(Device {
                id: take_string(device.GetId()?)?,
                name: name(&device)?,
                available: true,
            });
        }
        result.sort_by_cached_key(|device| device.name.to_lowercase());
        Ok(result)
    }
}

fn unsupported(error: &windows::core::Error) -> bool {
    matches!(
        error.code().0 as u32,
        0x80004002 | 0x80004001 | 0x80070490 | 0x80070032
    )
}

fn adjacent(part: &IPart, incoming: bool) -> Result<(u32, Option<IPart>)> {
    unsafe {
        let list = match if incoming {
            part.EnumPartsIncoming()
        } else {
            part.EnumPartsOutgoing()
        } {
            Ok(list) => list,
            Err(error) if error.code().0 as u32 == 0x80070490 => return Ok((0, None)),
            Err(error) => return Err(error.into()),
        };
        let count = list.GetCount()?;
        Ok((
            count,
            if !incoming && count == 1 {
                Some(list.GetPart(0)?)
            } else {
                None
            },
        ))
    }
}

fn hardware_controls(device: &IMMDevice) -> Result<Vec<IAudioMute>> {
    let result = (|| -> Result<Vec<IAudioMute>> {
        unsafe {
            let topology: IDeviceTopology = device.Activate(CLSCTX_ALL, None)?;
            if topology.GetConnectorCount()? != 1 {
                return Ok(vec![]);
            }
            let connector = topology.GetConnector(0)?;
            if !connector.IsConnected()?.as_bool() {
                return Ok(vec![]);
            }
            let mut part: IPart = connector.GetConnectedTo()?.cast()?;
            let mut visited = HashSet::new();
            let mut controls = Vec::new();
            for _ in 0..64 {
                let id = take_string(part.GetGlobalId()?)?;
                if !visited.insert(id) {
                    return Ok(vec![]);
                }
                let (incoming, _) = adjacent(&part, true)?;
                let (outgoing, next) = adjacent(&part, false)?;
                // Refuse shared/branched capture paths before exposing any writable control.
                if incoming > 1 || outgoing > 1 {
                    return Ok(vec![]);
                }
                let mut raw = std::ptr::null_mut();
                match part.Activate(CLSCTX_ALL.0, &IAudioMute::IID, Some(&mut raw)) {
                    Ok(()) => {
                        if raw.is_null() {
                            bail!("The hardware mute interface was null.");
                        }
                        let control = IAudioMute::from_raw(raw);
                        let _ = control.GetMute()?;
                        controls.push(control);
                    }
                    Err(error) if unsupported(&error) || error.code().0 as u32 == 0x80070057 => (),
                    Err(error) => return Err(error.into()),
                }
                match next {
                    Some(next) => part = next,
                    None => return Ok(controls),
                }
            }
            Ok(vec![])
        }
    })();
    match result {
        Err(error)
            if error
                .downcast_ref::<windows::core::Error>()
                .is_some_and(unsupported) =>
        {
            Ok(vec![])
        }
        result => result,
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback {
    window: isize,
    message: u32,
    pending: Arc<AtomicBool>,
}
impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, _: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        if !self.pending.swap(true, Ordering::AcqRel)
            && unsafe {
                PostMessageW(
                    Some(HWND(self.window as *mut _)),
                    self.message,
                    WPARAM(0),
                    LPARAM(0),
                )
            }
            .is_err()
        {
            self.pending.store(false, Ordering::Release);
        }
        Ok(())
    }
}

pub struct Endpoint {
    pub id: String,
    pub name: String,
    device: IMMDevice,
    volume: IAudioEndpointVolume,
    hardware: Vec<IAudioMute>,
    callback: Option<IAudioEndpointVolumeCallback>,
}

#[derive(Clone, Default, Serialize)]
pub struct Snapshot {
    pub available: bool,
    pub muted: bool,
    pub volume_minimized: bool,
    pub name: String,
    pub warning: Option<String>,
    pub volume: f32,
    pub hardware_controls: usize,
}

impl Endpoint {
    pub fn ensure_active(&self) -> Result<()> {
        if unsafe { self.device.GetState()? } != DEVICE_STATE_ACTIVE {
            bail!("The selected microphone is disconnected or disabled.");
        }
        Ok(())
    }
    pub fn open(id: &str) -> Result<Self> {
        unsafe {
            let device = enumerator()?.GetDevice(PCWSTR(wide(id).as_ptr()))?;
            let endpoint: IMMEndpoint = device.cast()?;
            if endpoint.GetDataFlow()? != eCapture {
                bail!("The selected device is not a microphone.");
            }
            if device.GetState()? != DEVICE_STATE_ACTIVE {
                bail!("The selected microphone is disconnected or disabled.");
            }
            let volume = device.Activate(CLSCTX_ALL, None)?;
            let hardware = hardware_controls(&device)?;
            let name = name(&device)?;
            Ok(Self {
                id: id.into(),
                name,
                device,
                volume,
                hardware,
                callback: None,
            })
        }
    }

    pub fn watch(&mut self, window: HWND, message: u32, pending: Arc<AtomicBool>) -> Result<()> {
        if self.callback.is_some() {
            return Ok(());
        }
        let callback: IAudioEndpointVolumeCallback = VolumeCallback {
            window: window.0 as isize,
            message,
            pending,
        }
        .into();
        unsafe {
            self.volume.RegisterControlChangeNotify(&callback)?;
        }
        self.callback = Some(callback);
        Ok(())
    }

    pub fn snapshot(&self, store: &impl RestoreStore) -> Result<Snapshot> {
        self.snapshot_with_backup(store.find(&self.id)?.as_ref())
    }

    pub fn snapshot_with_backup(&self, backup: Option<&VolumeBackup>) -> Result<Snapshot> {
        if backup.is_some_and(|backup| backup.device_id != self.id) {
            bail!("The saved volume belongs to another microphone.");
        }
        unsafe {
            if self.device.GetState()? != DEVICE_STATE_ACTIVE {
                bail!("The selected microphone is disconnected or disabled.");
            }
            let muted = self.volume.GetMute()?.as_bool();
            let mut warning = None;
            for control in &self.hardware {
                if control.GetMute()?.as_bool() != muted {
                    warning = Some("Windows and hardware mute controls disagree. Toggle again to synchronize them.".into());
                }
            }
            let backup = backup.is_some();
            let at_minimum = backup && self.at_minimum()?;
            if backup && !at_minimum {
                warning = Some(
                    "Compatibility mode needs attention: the microphone volume changed.".into(),
                );
            }
            Ok(Snapshot {
                available: true,
                muted: muted || at_minimum,
                volume_minimized: backup,
                name: self.name.clone(),
                warning,
                volume: self.volume.GetMasterVolumeLevelScalar()?,
                hardware_controls: self.hardware.len(),
            })
        }
    }

    pub fn toggle(&mut self, mode: MuteMode, store: &impl RestoreStore) -> Result<Snapshot> {
        if store.find(&self.id)?.is_some() {
            volume::unmute(self, store)?;
        } else {
            let mut muted = unsafe { self.volume.GetMute()?.as_bool() };
            for control in &self.hardware {
                muted |= unsafe { control.GetMute()?.as_bool() };
            }
            if mode == MuteMode::VolumeZero && !muted {
                volume::mute(self, store)?;
            } else {
                self.set_muted(!muted)?;
            }
        }
        self.snapshot(store)
    }

    fn set_hardware_muted(&self, muted: bool) -> Result<()> {
        for control in &self.hardware {
            unsafe {
                control.SetMute(muted, None)?;
                if control.GetMute()?.as_bool() != muted {
                    bail!("The hardware mute control did not accept the change.");
                }
            }
        }
        Ok(())
    }
}

impl VolumeEndpoint for Endpoint {
    fn id(&self) -> &str {
        &self.id
    }
    fn levels(&self) -> Result<Vec<f32>> {
        unsafe {
            let channels = self.volume.GetChannelCount()?;
            if channels == 0 || channels > 64 {
                bail!("The microphone channel count is unsupported.");
            }
            (0..channels)
                .map(|index| Ok(self.volume.GetChannelVolumeLevel(index)?))
                .collect()
        }
    }
    fn write_levels(&mut self, levels: &[f32]) -> Result<()> {
        unsafe {
            if self.volume.GetChannelCount()? as usize != levels.len() {
                bail!("Microphone channel count changed.");
            }
            for (index, level) in levels.iter().enumerate() {
                self.volume
                    .SetChannelVolumeLevel(index as u32, *level, std::ptr::null())?;
            }
        }
        Ok(())
    }
    fn minimize(&mut self) -> Result<()> {
        unsafe {
            self.volume
                .SetMasterVolumeLevelScalar(0.0, std::ptr::null())?;
        }
        Ok(())
    }
    fn at_minimum(&self) -> Result<bool> {
        unsafe {
            let master = self.volume.GetMasterVolumeLevelScalar()?;
            if !master.is_finite() || master > 0.00001 {
                return Ok(false);
            }
            let channels = self.volume.GetChannelCount()?;
            if channels == 0 || channels > 64 {
                bail!("The microphone channel count is unsupported.");
            }
            for index in 0..channels {
                let level = self.volume.GetChannelVolumeLevelScalar(index)?;
                if !level.is_finite() || level > 0.00001 {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
    fn set_muted(&mut self, muted: bool) -> Result<()> {
        unsafe {
            if muted {
                self.set_hardware_muted(true)?;
            }
            self.volume.SetMute(muted, std::ptr::null())?;
            if !muted {
                self.set_hardware_muted(false)?;
            }
            if self.volume.GetMute()?.as_bool() != muted {
                bail!("Windows did not confirm the requested mute state.");
            }
            for control in &self.hardware {
                if control.GetMute()?.as_bool() != muted {
                    bail!("The hardware mute control did not confirm the requested state.");
                }
            }
        }
        Ok(())
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.take() {
            unsafe {
                let _ = self.volume.UnregisterControlChangeNotify(&callback);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_endpoint_does_not_use_the_default_device() {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
            .ok()
            .unwrap();
        }
        let result = Endpoint::open("MuteMic.Rust.Tests.Missing.Endpoint");
        assert!(result.is_err());
        drop(result);
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}
