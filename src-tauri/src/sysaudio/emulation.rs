//! Mute emulation for devices without a real mute control (e.g. a built-in
//! microphone that only has a volume): muting sets the volume to 0 and
//! remembers the previous level; unmuting restores it. The remembered levels
//! survive restarts in `sysaudio-mutes.json` in the app's config folder.
//!
//! Emulated devices behave like devices with a real mute for consumers: they
//! report `can_mute: true`, `emulated: true`, and while muted `muted: true`
//! with `volume` = the level unmuting will restore. Setting the volume while
//! muted only changes that level (as with a real mute). Raising the volume
//! outside Lunchpad while emulated-muted counts as an unmute: the OS is the
//! source of truth.
//!
//! Owned by the engine's worker thread; it never runs under a lock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::engine::{AppStatus, Backend, Error, ErrorKind, MakeBackend};
use super::model::{Device, DeviceKind};

pub const FILE_NAME: &str = "sysaudio-mutes.json";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedLevel {
    id: String,
    kind: DeviceKind,
    volume: f32,
}

type Key = (String, DeviceKind);

pub struct Emulation {
    /// `None` keeps the levels in memory only (tests, no config folder).
    path: Option<PathBuf>,
    /// Emulated-muted devices and the level unmuting restores.
    levels: BTreeMap<Key, f32>,
}

/// Mute of this device has to be emulated: no real control, but a volume.
pub fn emulatable(device: &Device) -> bool {
    !device.can_mute && device.can_set_volume
}

impl Emulation {
    pub fn in_memory() -> Emulation {
        Emulation {
            path: None,
            levels: BTreeMap::new(),
        }
    }

    /// Loads the remembered levels from `dir`. A missing file is an empty
    /// store; an unreadable one is logged and replaced on the next change.
    pub fn load(dir: &Path) -> Emulation {
        let path = dir.join(FILE_NAME);
        let levels = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Vec<SavedLevel>>(&text) {
                Ok(saved) => saved
                    .into_iter()
                    .filter(|s| {
                        super::model::valid_device_id(&s.id) && (0.0..=1.0).contains(&s.volume)
                    })
                    .map(|s| ((s.id, s.kind), s.volume))
                    .collect(),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "remembered mute levels unreadable, starting empty");
                    BTreeMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "remembered mute levels could not be read, starting empty");
                BTreeMap::new()
            }
        };
        Emulation {
            path: Some(path),
            levels,
        }
    }

    #[cfg(test)]
    pub fn is_muted(&self, device: &Device) -> bool {
        self.levels.contains_key(&key(device))
    }

    /// Turns the backend's snapshot into what consumers see: emulatable
    /// devices get `can_mute`/`emulated`, emulated-muted ones `muted` and their
    /// remembered level. An external volume change ends an emulated mute.
    pub fn overlay(&mut self, devices: &mut [Device]) {
        let mut changed = false;
        for device in devices.iter_mut().filter(|d| emulatable(d)) {
            device.emulated = true;
            device.can_mute = true;
            device.muted = false;
            let key = key(device);
            let Some(&level) = self.levels.get(&key) else {
                continue;
            };
            if device.volume > 0.0 {
                // Someone turned it up outside Lunchpad: it is not muted any more.
                self.levels.remove(&key);
                changed = true;
            } else {
                device.muted = true;
                device.volume = level;
            }
        }
        if changed {
            if let Err(e) = self.save() {
                tracing::warn!(error = %e, "external unmute could not be persisted");
            }
        }
    }

    /// Emulated mute or unmute of `device` (an entry of the overlaid snapshot).
    pub fn set_muted(
        &mut self,
        backend: &mut dyn Backend,
        device: &Device,
        muted: bool,
    ) -> Result<(), Error> {
        let key = key(device);
        match (muted, self.levels.get(&key).copied()) {
            (true, Some(_)) | (false, None) => Ok(()),
            (true, None) => {
                // Remember first: a crash after the write must still be able to restore.
                self.levels.insert(key.clone(), device.volume);
                if let Err(e) = self.save() {
                    self.levels.remove(&key);
                    return Err(e);
                }
                backend.set_device_volume(&device.id, device.kind, 0.0).inspect_err(|_| {
                    self.levels.remove(&key);
                    if let Err(e) = self.save() {
                        tracing::warn!(error = %e, "failed mute rollback could not be persisted");
                    }
                })
            }
            (false, Some(level)) => {
                backend.set_device_volume(&device.id, device.kind, level)?;
                self.levels.remove(&key);
                self.save()
            }
        }
    }

    /// A volume set while emulated-muted changes the level unmuting restores,
    /// and nothing else. Returns false when the device is not emulated-muted.
    pub fn remember(&mut self, device: &Device, volume: f32) -> Result<bool, Error> {
        match self.levels.get_mut(&key(device)) {
            Some(level) => {
                let old = *level;
                *level = volume;
                if let Err(e) = self.save() {
                    self.levels.insert(key(device), old);
                    return Err(e);
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Save before zeroing the device: a persistence failure must not lose
    /// the only copy of the level needed to unmute after a restart.
    fn save(&self) -> Result<(), Error> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let saved: Vec<SavedLevel> = self
            .levels
            .iter()
            .map(|((id, kind), volume)| SavedLevel {
                id: id.clone(),
                kind: *kind,
                volume: *volume,
            })
            .collect();
        let result = serde_json::to_string_pretty(&saved)
            .map_err(std::io::Error::other)
            .and_then(|json| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let tmp = path.with_extension("json.tmp");
                use std::io::Write;
                let mut file = std::fs::File::create(&tmp)?;
                file.write_all(json.as_bytes())?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(&tmp, path)
            });
        result.map_err(|e| {
            Error::new(
                ErrorKind::Native,
                format!("cannot save mute restore levels: {e}"),
            )
        })
    }
}

fn key(device: &Device) -> Key {
    (device.id.clone(), device.kind)
}

/// Adds emulated mute without changing the platform-specific controls. Both
/// the store and every backend call stay owned by the same worker.
pub fn wrap(make: MakeBackend, config_dir: Option<PathBuf>) -> MakeBackend {
    Box::new(move |notifier| {
        let backend = make(notifier)?;
        let emulation = config_dir
            .as_deref()
            .map(Emulation::load)
            .unwrap_or_else(Emulation::in_memory);
        Ok(Box::new(Emulated { backend, emulation }))
    })
}

struct Emulated {
    backend: Box<dyn Backend>,
    emulation: Emulation,
}

impl Emulated {
    fn device(&mut self, id: &str, kind: DeviceKind) -> Result<Device, Error> {
        self.devices()?
            .into_iter()
            .find(|d| d.id == id && d.kind == kind)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "audio device disappeared"))
    }
}

impl Backend for Emulated {
    fn configure(&mut self, options: super::Options) -> Result<(), Error> {
        self.backend.configure(options)
    }
    fn status(&self) -> AppStatus {
        self.backend.status()
    }
    fn poll_interval(&self) -> std::time::Duration {
        self.backend.poll_interval()
    }
    fn snapshot(&mut self) -> Result<Vec<super::model::App>, Error> {
        self.backend.snapshot()
    }
    fn set_volume(&mut self, app: &str, volume: f32) -> Result<(), Error> {
        self.backend.set_volume(app, volume)
    }
    fn set_mute(&mut self, app: &str, muted: bool) -> Result<(), Error> {
        self.backend.set_mute(app, muted)
    }
    fn devices(&mut self) -> Result<Vec<Device>, Error> {
        let mut devices = self.backend.devices()?;
        self.emulation.overlay(&mut devices);
        Ok(devices)
    }
    fn set_device_volume(&mut self, id: &str, kind: DeviceKind, volume: f32) -> Result<(), Error> {
        let device = self.device(id, kind)?;
        if device.emulated && self.emulation.remember(&device, volume)? {
            return Ok(());
        }
        self.backend.set_device_volume(id, kind, volume)
    }
    fn set_device_mute(&mut self, id: &str, kind: DeviceKind, muted: bool) -> Result<(), Error> {
        let device = self.device(id, kind)?;
        if device.emulated {
            self.emulation
                .set_muted(self.backend.as_mut(), &device, muted)
        } else {
            self.backend.set_device_mute(id, kind, muted)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::engine::{AppStatus, BackendKind, ErrorKind};
    use super::super::model::App;
    use super::*;
    use std::time::Duration;

    fn mic(volume: f32) -> Device {
        Device {
            id: "BuiltInMic".into(),
            name: "Built-in mic".into(),
            kind: DeviceKind::Input,
            volume,
            muted: false,
            is_default: true,
            can_set_volume: true,
            can_mute: false,
            emulated: false,
        }
    }

    /// Records device volume writes; can be told to fail.
    #[derive(Default)]
    struct Volumes {
        writes: Vec<f32>,
        fail: bool,
    }

    impl Backend for Volumes {
        fn status(&self) -> AppStatus {
            AppStatus::unavailable(BackendKind::None, "")
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_secs(60)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            Ok(Vec::new())
        }
        fn set_volume(&mut self, _: &str, _: f32) -> Result<(), Error> {
            unreachable!()
        }
        fn set_mute(&mut self, _: &str, _: bool) -> Result<(), Error> {
            unreachable!()
        }
        fn set_device_volume(&mut self, _: &str, _: DeviceKind, volume: f32) -> Result<(), Error> {
            if self.fail {
                return Err(Error::new(ErrorKind::Native, "write failed"));
            }
            self.writes.push(volume);
            Ok(())
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lunchpad-sysaudio-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn overlaid(emulation: &mut Emulation, device: Device) -> Device {
        let mut devices = [device];
        emulation.overlay(&mut devices);
        devices[0].clone()
    }

    #[test]
    fn only_mute_less_devices_with_a_volume_are_emulated() {
        let mut emulation = Emulation::in_memory();
        let real = Device {
            can_mute: true,
            ..mic(0.5)
        };
        let fixed = Device {
            can_set_volume: false,
            ..mic(0.5)
        };
        let mut devices = [mic(0.5), real.clone(), fixed.clone()];
        emulation.overlay(&mut devices);
        assert!(devices[0].emulated && devices[0].can_mute && !devices[0].muted);
        assert_eq!(devices[1], real, "a real mute control is left alone");
        assert_eq!(devices[2], fixed, "nothing to emulate with");
        assert!(
            !overlaid(&mut emulation, mic(0.0)).muted,
            "zero alone does not establish an emulated mute"
        );
    }

    #[test]
    fn mute_zeroes_the_volume_and_unmute_restores_the_level() {
        let mut emulation = Emulation::in_memory();
        let mut backend = Volumes::default();
        let device = overlaid(&mut emulation, mic(0.6));
        emulation.set_muted(&mut backend, &device, true).unwrap();
        // The OS now reads 0; consumers see muted at the remembered level.
        let muted = overlaid(&mut emulation, mic(0.0));
        assert!(muted.muted && muted.emulated);
        assert_eq!(muted.volume, 0.6);
        // Muting again must not remember 0 as the level.
        emulation.set_muted(&mut backend, &muted, true).unwrap();
        // A volume set while muted only changes what unmute restores.
        assert!(emulation.remember(&muted, 0.4).unwrap());
        assert_eq!(overlaid(&mut emulation, mic(0.0)).volume, 0.4);
        emulation.set_muted(&mut backend, &muted, false).unwrap();
        assert_eq!(backend.writes, [0.0, 0.4]);
        let unmuted = overlaid(&mut emulation, mic(0.4));
        assert!(!unmuted.muted && unmuted.emulated);
        // Unmuting what is not muted changes nothing.
        emulation.set_muted(&mut backend, &unmuted, false).unwrap();
        assert!(!emulation.remember(&unmuted, 0.9).unwrap());
        assert_eq!(backend.writes, [0.0, 0.4]);
    }

    #[test]
    fn a_failed_write_keeps_the_previous_state() {
        let mut emulation = Emulation::in_memory();
        let device = overlaid(&mut emulation, mic(0.6));
        let mut failing = Volumes {
            fail: true,
            ..Volumes::default()
        };
        assert!(emulation.set_muted(&mut failing, &device, true).is_err());
        assert!(
            !emulation.is_muted(&device),
            "a mute that did not happen is not remembered"
        );

        let mut backend = Volumes::default();
        emulation.set_muted(&mut backend, &device, true).unwrap();
        assert!(emulation.set_muted(&mut failing, &device, false).is_err());
        assert!(
            emulation.is_muted(&device),
            "the level survives a failed unmute"
        );
    }

    #[test]
    fn turning_it_up_elsewhere_ends_the_emulated_mute() {
        let mut emulation = Emulation::in_memory();
        let device = overlaid(&mut emulation, mic(0.6));
        emulation
            .set_muted(&mut Volumes::default(), &device, true)
            .unwrap();
        let raised = overlaid(&mut emulation, mic(0.3));
        assert!(!raised.muted);
        assert_eq!(raised.volume, 0.3, "the OS value, not the remembered one");
        assert!(!emulation.is_muted(&raised));
    }

    #[test]
    fn remembered_levels_survive_a_restart_and_a_damaged_file_is_survivable() {
        let dir = temp_dir();
        let mut emulation = Emulation::load(&dir);
        let device = overlaid(&mut emulation, mic(0.7));
        emulation
            .set_muted(&mut Volumes::default(), &device, true)
            .unwrap();
        drop(emulation);

        let mut restarted = Emulation::load(&dir);
        let device = overlaid(&mut restarted, mic(0.0));
        assert!(device.muted);
        assert_eq!(device.volume, 0.7);
        let mut backend = Volumes::default();
        restarted.set_muted(&mut backend, &device, false).unwrap();
        assert_eq!(backend.writes, [0.7]);
        assert!(
            !Emulation::load(&dir).is_muted(&device),
            "the unmute is persisted too"
        );

        std::fs::write(dir.join(FILE_NAME), "{not json").unwrap();
        let mut damaged = Emulation::load(&dir);
        assert!(!overlaid(&mut damaged, mic(0.0)).muted);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_persist_never_zeroes_the_device_or_loses_the_previous_restore_level() {
        let dir = temp_dir();
        let mut emulation = Emulation::load(&dir);
        let device = overlaid(&mut emulation, mic(0.7));
        std::fs::create_dir(dir.join(FILE_NAME)).unwrap(); // cannot replace a directory with the file
        let mut backend = Volumes::default();
        assert!(emulation.set_muted(&mut backend, &device, true).is_err());
        assert!(backend.writes.is_empty());
        assert!(!emulation.is_muted(&device));
        std::fs::remove_dir(dir.join(FILE_NAME)).unwrap();
        emulation.set_muted(&mut backend, &device, true).unwrap();
        std::fs::remove_file(dir.join(FILE_NAME)).unwrap();
        std::fs::create_dir(dir.join(FILE_NAME)).unwrap();
        assert!(emulation.remember(&device, 0.2).is_err());
        assert_eq!(overlaid(&mut emulation, mic(0.0)).volume, 0.7);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn duplex_keys_do_not_share_mute_and_unplugging_does_not_forget_it() {
        let dir = temp_dir();
        let mut emulation = Emulation::load(&dir);
        let input = overlaid(&mut emulation, mic(0.7));
        emulation
            .set_muted(&mut Volumes::default(), &input, true)
            .unwrap();
        let output = Device {
            kind: DeviceKind::Output,
            ..mic(0.0)
        };
        assert!(!overlaid(&mut emulation, output).muted);
        emulation.overlay(&mut []); // device disappears while muted
        assert!(overlaid(&mut Emulation::load(&dir), mic(0.0)).muted);
        // Someone restores a positive volume while Lunchpad is stopped.
        assert!(!overlaid(&mut Emulation::load(&dir), mic(0.4)).muted);
        assert!(!Emulation::load(&dir).is_muted(&input));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
