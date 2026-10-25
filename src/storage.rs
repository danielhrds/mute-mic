use crate::input;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub enum InputMode {
    #[default]
    RawInput,
    Compatibility,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub enum MuteMode {
    Endpoint,
    #[default]
    VolumeZero,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EnumValue {
    Name(String),
    Number(i64),
}

impl<'de> Deserialize<'de> for InputMode {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        match EnumValue::deserialize(deserializer)? {
            EnumValue::Number(1) => Ok(Self::Compatibility),
            EnumValue::Number(_) => Ok(Self::RawInput),
            EnumValue::Name(name) if name.eq_ignore_ascii_case("RawInput") => Ok(Self::RawInput),
            EnumValue::Name(name) if name.eq_ignore_ascii_case("Compatibility") => {
                Ok(Self::Compatibility)
            }
            _ => Err(serde::de::Error::custom("Unknown input mode.")),
        }
    }
}

impl<'de> Deserialize<'de> for MuteMode {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        match EnumValue::deserialize(deserializer)? {
            EnumValue::Number(1) => Ok(Self::VolumeZero),
            EnumValue::Number(_) => Ok(Self::Endpoint),
            EnumValue::Name(name) if name.eq_ignore_ascii_case("Endpoint") => Ok(Self::Endpoint),
            EnumValue::Name(name) if name.eq_ignore_ascii_case("VolumeZero") => {
                Ok(Self::VolumeZero)
            }
            _ => Err(serde::de::Error::custom("Unknown mute mode.")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Settings {
    pub device_id: Option<String>,
    pub device_name: Option<String>,
    #[serde(deserialize_with = "deserialize_hotkey")]
    pub hotkey: u16,
    pub input_mode: InputMode,
    pub play_sounds: bool,
    pub mute_mode: MuteMode,
    pub start_with_windows: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_id: None,
            device_name: None,
            hotkey: 0x77,
            input_mode: InputMode::RawInput,
            play_sounds: true,
            mute_mode: MuteMode::VolumeZero,
            start_with_windows: false,
        }
    }
}

impl Settings {
    pub fn validate(mut self) -> Self {
        if !input::supported(self.hotkey) {
            self.hotkey = 0x77;
        }
        if self
            .device_id
            .as_ref()
            .is_some_and(|id| id.trim().is_empty())
        {
            self.device_id = None;
        }
        self
    }

    pub fn load(directory: &Path) -> (Self, Option<String>) {
        match fs::read(directory.join("settings.json")) {
            Ok(bytes) => match serde_json::from_slice::<Self>(json_bytes(&bytes)) {
                Ok(settings) => (settings.validate(), None),
                Err(_) => (
                    Self::default(),
                    Some(
                        "Settings could not be loaded. Choose your headset again and save.".into(),
                    ),
                ),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => (Self::default(), None),
            Err(error) => (
                Self::default(),
                Some(format!("Settings could not be read: {error}")),
            ),
        }
    }

    pub fn save(&self, directory: &Path) -> Result<()> {
        atomic_json(&directory.join("settings.json"), &self.clone().validate())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeBackup {
    pub device_id: String,
    pub channel_levels_db: Vec<f32>,
}

pub trait RestoreStore {
    fn find(&self, id: &str) -> Result<Option<VolumeBackup>>;
    fn remember(&self, backup: VolumeBackup) -> Result<()>;
    fn remove(&self, id: &str) -> Result<()>;
}

pub struct VolumeStore {
    path: PathBuf,
}
impl VolumeStore {
    pub fn new(directory: &Path) -> Self {
        Self {
            path: directory.join("volume-restore.json"),
        }
    }
    fn read(&self) -> Result<BTreeMap<String, VolumeBackup>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(error) => {
                return Err(error)
                    .context("Saved volume could not be read; no volume change will be made.");
            }
        };
        let entries = serde_json::from_slice::<RecoveryEntries>(json_bytes(&bytes))
            .context("Saved volume is corrupt. The recovery file was preserved; no volume change will be made.")?;
        for (id, backup) in &entries.0 {
            validate_backup(id, backup)?;
        }
        Ok(entries.0)
    }
}

struct RecoveryEntries(BTreeMap<String, VolumeBackup>);

impl<'de> Deserialize<'de> for RecoveryEntries {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = RecoveryEntries;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("unique microphone recovery entries")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut entries = BTreeMap::new();
                while let Some((id, backup)) = map.next_entry::<String, VolumeBackup>()? {
                    if entries.insert(id, backup).is_some() {
                        return Err(serde::de::Error::custom(
                            "Duplicate microphone recovery entry.",
                        ));
                    }
                }
                Ok(RecoveryEntries(entries))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

impl RestoreStore for VolumeStore {
    fn find(&self, id: &str) -> Result<Option<VolumeBackup>> {
        Ok(self.read()?.remove(id))
    }
    fn remember(&self, backup: VolumeBackup) -> Result<()> {
        validate_backup(&backup.device_id, &backup)?;
        let mut entries = self.read()?;
        if entries.contains_key(&backup.device_id) {
            return Ok(());
        }
        entries.insert(backup.device_id.clone(), backup);
        atomic_json(&self.path, &entries)
    }
    fn remove(&self, id: &str) -> Result<()> {
        let mut entries = self.read()?;
        if entries.remove(id).is_some() {
            atomic_json(&self.path, &entries)?;
        }
        Ok(())
    }
}

fn validate_backup(id: &str, backup: &VolumeBackup) -> Result<()> {
    if id.trim().is_empty()
        || backup.device_id != id
        || backup.channel_levels_db.is_empty()
        || backup.channel_levels_db.len() > 64
        || backup
            .channel_levels_db
            .iter()
            .any(|level| !level.is_finite())
    {
        bail!(
            "Saved volume is invalid. The recovery file was preserved; no volume change will be made."
        );
    }
    Ok(())
}

fn json_bytes(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
}

fn deserialize_hotkey<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u16, D::Error> {
    let key = i64::deserialize(deserializer)?;
    Ok(u16::try_from(key)
        .ok()
        .filter(|key| input::supported(*key))
        .unwrap_or(0x77))
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = path.parent().context("Missing settings directory.")?;
    fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(
        "mutemic.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let mut file = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut file, value)?;
        file.flush()?;
        file.get_ref().sync_all()?;
        drop(file);
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::Storage::FileSystem::{
                MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
            },
            core::PCWSTR,
        };
        let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
pub mod tests {
    use super::*;
    pub fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "MuteMic.Rust.Tests.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn settings_round_trip_keeps_endpoint_and_key() {
        let directory = temp_dir();
        let expected = Settings {
            device_id: Some("exact-headset".into()),
            hotkey: 0x78,
            input_mode: InputMode::Compatibility,
            ..Settings::default()
        };
        expected.save(&directory).unwrap();
        assert_eq!(Settings::load(&directory), (expected, None));
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn corrupt_and_null_settings_are_preserved() {
        let directory = temp_dir();
        for contents in ["{broken", "null", "{\"InputMode\":\"Invalid\"}"] {
            fs::write(directory.join("settings.json"), contents).unwrap();
            let (settings, warning) = Settings::load(&directory);
            assert!(settings.device_id.is_none() && warning.is_some());
            assert_eq!(
                fs::read_to_string(directory.join("settings.json")).unwrap(),
                contents
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn missing_settings_never_choose_a_microphone() {
        let directory = temp_dir();
        let (settings, warning) = Settings::load(&directory);
        assert!(settings.device_id.is_none() && warning.is_none());
        assert_eq!(settings.hotkey, 0x77);
        assert_eq!(input::key_label(settings.hotkey), "F8");
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn saved_shortcut_is_kept_instead_of_replaced_by_the_default() {
        let directory = temp_dir();
        Settings {
            hotkey: 0x87,
            ..Settings::default()
        }
        .save(&directory)
        .unwrap();
        assert_eq!(Settings::load(&directory).0.hotkey, 0x87);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn bom_and_invalid_shortcut_preserve_selected_endpoint() {
        let directory = temp_dir();
        fs::write(
            directory.join("settings.json"),
            b"\xef\xbb\xbf{\"DeviceId\":\"headset\",\"Hotkey\":-1}",
        )
        .unwrap();
        let (settings, warning) = Settings::load(&directory);
        assert!(warning.is_none());
        assert_eq!(settings.device_id.as_deref(), Some("headset"));
        assert_eq!(settings.hotkey, 0x77);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn existing_backup_is_never_overwritten() {
        let directory = temp_dir();
        let store = VolumeStore::new(&directory);
        store
            .remember(VolumeBackup {
                device_id: "headset".into(),
                channel_levels_db: vec![-3.5, -8.25],
            })
            .unwrap();
        VolumeStore::new(&directory)
            .remember(VolumeBackup {
                device_id: "headset".into(),
                channel_levels_db: vec![-90.0, -90.0],
            })
            .unwrap();
        assert_eq!(
            store.find("headset").unwrap().unwrap().channel_levels_db,
            vec![-3.5, -8.25]
        );
        assert!(store.find("other").unwrap().is_none());
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn invalid_numeric_modes_keep_the_selected_endpoint() {
        let settings: Settings =
            serde_json::from_str(r#"{"DeviceId":"headset","InputMode":999,"MuteMode":999}"#)
                .unwrap();
        assert_eq!(settings.device_id.as_deref(), Some("headset"));
        assert_eq!(settings.input_mode, InputMode::RawInput);
        assert_eq!(settings.mute_mode, MuteMode::Endpoint);
    }
    #[test]
    fn existing_recovery_format_is_compatible() {
        let directory = temp_dir();
        fs::write(
            directory.join("volume-restore.json"),
            r#"{"headset":{"DeviceId":"headset","ChannelLevelsDb":[-3.5,-8.25]}}"#,
        )
        .unwrap();
        let backup = VolumeStore::new(&directory)
            .find("headset")
            .unwrap()
            .unwrap();
        assert_eq!(backup.channel_levels_db, vec![-3.5, -8.25]);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn corrupt_backup_blocks_changes() {
        let directory = temp_dir();
        fs::write(directory.join("volume-restore.json"), "null").unwrap();
        let store = VolumeStore::new(&directory);
        assert!(store.find("headset").is_err());
        assert!(
            store
                .remember(VolumeBackup {
                    device_id: "headset".into(),
                    channel_levels_db: vec![-3.0]
                })
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(directory.join("volume-restore.json")).unwrap(),
            "null"
        );
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn duplicate_recovery_entries_cannot_replace_original_levels() {
        let directory = temp_dir();
        let path = directory.join("volume-restore.json");
        let contents = r#"{"headset":{"DeviceId":"headset","ChannelLevelsDb":[-3.5]},"headset":{"DeviceId":"headset","ChannelLevelsDb":[-90.0]}}"#;
        fs::write(&path, contents).unwrap();
        let store = VolumeStore::new(&directory);
        assert!(store.find("headset").is_err());
        assert!(store.remove("headset").is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), contents);
        fs::remove_dir_all(directory).unwrap();
    }
}
