//! Tauri command layer: thin wrappers between the frontend and the core.

pub mod debug;
pub mod desktop;
pub mod device;
pub mod diagnostics;
pub mod finetune;
pub mod hub;
pub mod macros;
pub mod media;
pub mod profile;
pub mod remote;
pub mod settings;
pub mod wavelink;

use crate::audio::AudioHandle;
use crate::config::SharedSettings;
use crate::input::SharedKeyboard;
use crate::macros::MacroEngine;
use crate::midi::SharedManager;
use crate::obs::ObsHandle;
use crate::profile::ProfileStore;
use crate::slobs::SlobsHandle;
use crate::speech::SpeechHandle;
use parking_lot::Mutex;
use std::sync::Arc;

pub struct AppState {
    pub manager: SharedManager,
    pub profile: Arc<Mutex<ProfileStore>>,
    pub settings: SharedSettings,
    pub engine: MacroEngine,
    pub keyboard: SharedKeyboard,
    pub audio: AudioHandle,
    pub speech: SpeechHandle,
    pub obs: ObsHandle,
    pub slobs: SlobsHandle,
    pub home_assistant: crate::homeassistant::HaHandle,
    pub sysaudio: crate::sysaudio::SystemAudio,
    pub hub: crate::hub::HubHandle,
    pub system_audio: crate::system_audio::SystemAudio,
    pub audio_bridge: Mutex<Option<crate::system_audio_bridge::BridgeHandle>>,
    pub audio_settings_write: tokio::sync::Mutex<()>,
    pub wave_link: crate::wavelink::WaveLink,
    pub wave_link_bridge: Mutex<Option<crate::wavelink::WaveLinkBridge>>,
    pub fine_tune: crate::finetune::FineTune,
    pub fine_tune_bridge: Mutex<Option<crate::finetune::FineTuneBridge>>,
    pub remote: crate::remote::RemoteService,
    /// Profiles before each edit made from the UI, newest last (undo).
    pub history: Mutex<Vec<crate::profile::Profile>>,
    /// Profiles undone, newest last (redo); cleared by the next edit.
    pub redo: Mutex<Vec<crate::profile::Profile>>,
}

pub type CmdResult<T> = Result<T, String>;

pub fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub use debug::*;
pub use desktop::*;
pub use device::*;
pub use diagnostics::*;
pub use finetune::*;
pub use hub::*;
pub use macros::*;
pub use media::*;
pub use profile::*;
pub use remote::*;
pub use settings::*;
pub use wavelink::*;
