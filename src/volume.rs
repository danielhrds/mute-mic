use crate::storage::{RestoreStore, VolumeBackup};
use anyhow::{Result, bail};

pub trait VolumeEndpoint {
    fn id(&self) -> &str;
    fn levels(&self) -> Result<Vec<f32>>;
    fn write_levels(&mut self, levels: &[f32]) -> Result<()>;
    fn minimize(&mut self) -> Result<()>;
    fn at_minimum(&self) -> Result<bool>;
    fn set_muted(&mut self, muted: bool) -> Result<()>;
}

pub fn mute(endpoint: &mut impl VolumeEndpoint, store: &impl RestoreStore) -> Result<()> {
    // Persist the original levels before changing any audio control.
    if store.find(endpoint.id())?.is_none() {
        store.remember(VolumeBackup {
            device_id: endpoint.id().into(),
            channel_levels_db: endpoint.levels()?,
        })?;
    }
    endpoint.minimize()?;
    if !endpoint.at_minimum()? {
        bail!("The microphone did not accept minimum volume.");
    }
    endpoint.set_muted(true)
}

pub fn unmute(endpoint: &mut impl VolumeEndpoint, store: &impl RestoreStore) -> Result<()> {
    let Some(backup) = store.find(endpoint.id())? else {
        return endpoint.set_muted(false);
    };
    if endpoint.levels()?.len() != backup.channel_levels_db.len() {
        bail!("The microphone channel layout changed. The original volume backup was preserved.");
    }
    endpoint.set_muted(true)?;
    let restore = (|| {
        endpoint.write_levels(&backup.channel_levels_db)?;
        let actual = endpoint.levels()?;
        if actual.len() != backup.channel_levels_db.len()
            || actual
                .iter()
                .zip(&backup.channel_levels_db)
                .any(|(actual, expected)| !actual.is_finite() || (actual - expected).abs() > 0.01)
        {
            bail!("The microphone did not restore its saved volume.");
        }
        Ok(())
    })();
    if let Err(error) = restore {
        let _ = endpoint.minimize();
        return Err(error);
    }
    endpoint.set_muted(false)?;
    store.remove(endpoint.id())
}

pub fn maintain(endpoint: &mut impl VolumeEndpoint, backup: Option<&VolumeBackup>) -> Result<bool> {
    if let Some(backup) = backup {
        if backup.device_id != endpoint.id() {
            bail!("The saved volume belongs to another microphone.");
        }
    } else {
        return Ok(false);
    }
    if !endpoint.at_minimum()? {
        endpoint.minimize()?;
        if !endpoint.at_minimum()? {
            bail!(
                "Another application changed the volume and minimum volume could not be restored."
            );
        }
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{VolumeStore, tests::temp_dir};
    struct Fake {
        levels: Vec<f32>,
        muted: bool,
        writes: u32,
        fail_restore: bool,
        fail_unmute: bool,
        override_levels: Option<Vec<f32>>,
        ignore_minimum: bool,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                levels: vec![-3.5, -8.25],
                muted: false,
                writes: 0,
                fail_restore: false,
                fail_unmute: false,
                override_levels: None,
                ignore_minimum: false,
            }
        }
    }
    impl VolumeEndpoint for Fake {
        fn id(&self) -> &str {
            "headset"
        }
        fn levels(&self) -> Result<Vec<f32>> {
            Ok(self.levels.clone())
        }
        fn write_levels(&mut self, levels: &[f32]) -> Result<()> {
            if self.fail_restore {
                bail!("Rejected restore.");
            }
            self.writes += 1;
            self.levels = self
                .override_levels
                .clone()
                .unwrap_or_else(|| levels.to_vec());
            Ok(())
        }
        fn minimize(&mut self) -> Result<()> {
            self.writes += 1;
            if !self.ignore_minimum {
                self.levels.fill(-90.0);
            }
            Ok(())
        }
        fn at_minimum(&self) -> Result<bool> {
            Ok(self.levels.iter().all(|level| *level == -90.0))
        }
        fn set_muted(&mut self, muted: bool) -> Result<()> {
            if !muted && self.fail_unmute {
                bail!("Rejected unmute.");
            }
            self.writes += 1;
            self.muted = muted;
            Ok(())
        }
    }
    #[test]
    fn restart_restores_exact_channels() {
        let directory = temp_dir();
        let mut endpoint = Fake::new();
        mute(&mut endpoint, &VolumeStore::new(&directory)).unwrap();
        let restarted = VolumeStore::new(&directory);
        mute(&mut endpoint, &restarted).unwrap();
        unmute(&mut endpoint, &restarted).unwrap();
        assert_eq!(endpoint.levels, vec![-3.5, -8.25]);
        assert!(!endpoint.muted && restarted.find("headset").unwrap().is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn corrupt_store_prevents_audio_writes() {
        let directory = temp_dir();
        std::fs::write(directory.join("volume-restore.json"), "{broken").unwrap();
        let mut endpoint = Fake::new();
        assert!(mute(&mut endpoint, &VolumeStore::new(&directory)).is_err());
        assert_eq!(endpoint.writes, 0);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn failed_save_prevents_audio_writes() {
        struct Failing;
        impl RestoreStore for Failing {
            fn find(&self, _: &str) -> Result<Option<VolumeBackup>> {
                Ok(None)
            }
            fn remember(&self, _: VolumeBackup) -> Result<()> {
                bail!("Disk full.");
            }
            fn remove(&self, _: &str) -> Result<()> {
                unreachable!()
            }
        }
        let mut endpoint = Fake::new();
        assert!(mute(&mut endpoint, &Failing).is_err());
        assert_eq!(endpoint.writes, 0);
    }
    #[test]
    fn rejected_restore_keeps_backup() {
        let directory = temp_dir();
        let store = VolumeStore::new(&directory);
        let mut endpoint = Fake::new();
        mute(&mut endpoint, &store).unwrap();
        endpoint.fail_restore = true;
        assert!(unmute(&mut endpoint, &store).is_err());
        assert!(
            endpoint.muted
                && endpoint.at_minimum().unwrap()
                && store.find("headset").unwrap().is_some()
        );
        endpoint.fail_restore = false;
        endpoint.fail_unmute = true;
        assert!(unmute(&mut endpoint, &store).is_err());
        assert!(store.find("headset").unwrap().is_some());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn malformed_readback_never_discards_backup() {
        for actual in [vec![-3.5], vec![f32::NAN, -8.25]] {
            let directory = temp_dir();
            let store = VolumeStore::new(&directory);
            let mut endpoint = Fake::new();
            mute(&mut endpoint, &store).unwrap();
            endpoint.override_levels = Some(actual);
            assert!(unmute(&mut endpoint, &store).is_err());
            assert!(
                endpoint.muted
                    && endpoint.at_minimum().unwrap()
                    && store.find("headset").unwrap().is_some()
            );
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
    #[test]
    fn changed_layout_is_rejected_before_writes() {
        let directory = temp_dir();
        let store = VolumeStore::new(&directory);
        let mut endpoint = Fake::new();
        mute(&mut endpoint, &store).unwrap();
        endpoint.levels.pop();
        let writes = endpoint.writes;
        assert!(unmute(&mut endpoint, &store).is_err());
        assert_eq!(endpoint.writes, writes);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn maintenance_preserves_original_levels() {
        let directory = temp_dir();
        let store = VolumeStore::new(&directory);
        let mut endpoint = Fake::new();
        mute(&mut endpoint, &store).unwrap();
        endpoint.levels.fill(-1.0);
        assert!(maintain(&mut endpoint, store.find("headset").unwrap().as_ref()).unwrap());
        assert_eq!(
            store.find("headset").unwrap().unwrap().channel_levels_db,
            vec![-3.5, -8.25]
        );
        unmute(&mut endpoint, &store).unwrap();
        let writes = endpoint.writes;
        assert!(!maintain(&mut endpoint, store.find("headset").unwrap().as_ref()).unwrap());
        assert_eq!(endpoint.writes, writes);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn rejected_minimum_retains_backup() {
        let directory = temp_dir();
        let store = VolumeStore::new(&directory);
        let mut endpoint = Fake::new();
        endpoint.ignore_minimum = true;
        assert!(mute(&mut endpoint, &store).is_err());
        assert!(store.find("headset").unwrap().is_some());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn maintenance_refuses_recovery_for_another_endpoint() {
        let mut endpoint = Fake::new();
        let backup = VolumeBackup {
            device_id: "other".into(),
            channel_levels_db: vec![-5.0],
        };
        assert!(maintain(&mut endpoint, Some(&backup)).is_err());
        assert_eq!(endpoint.writes, 0);
    }
}
