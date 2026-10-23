//! The phone remote's WebSocket protocol (version [`PROTOCOL_VERSION`]), JSON text
//! messages tagged by `type`, and what the remote may do: press and release pads,
//! move a fader of the page on screen, switch pages. Nothing else is accepted.

use crate::live::LiveStatus;
use crate::midi::render::{build_frame, RenderInputs};
use crate::midi::{
    palette, ConnectedDevice, ControlEvent, ControlKind, Layout, LedColor, PadShape, RenderView,
};
use crate::profile::{FaderDirection, Look, ProfileStore};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const PROTOCOL_VERSION: u32 = 1;

/// Longest device name kept from a pairing request.
pub const MAX_NAME: usize = 40;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum ClientMessage {
    /// First message of a phone not paired yet.
    #[serde(rename_all = "camelCase")]
    Pair {
        v: u32,
        code: String,
        #[serde(default)]
        name: String,
    },
    /// First message of a paired phone.
    #[serde(rename_all = "camelCase")]
    Auth {
        v: u32,
        token: String,
    },
    Press {
        x: u8,
        y: u8,
    },
    Release {
        x: u8,
        y: u8,
    },
    /// `value`: 0..1 over the fader's travel (min..max).
    #[serde(rename_all = "camelCase")]
    SetFader {
        fader_id: String,
        value: f64,
    },
    #[serde(rename_all = "camelCase")]
    SwitchPage {
        page_id: String,
    },
    /// Keep-alive from the page (browsers cannot send WebSocket pings themselves).
    Ping,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    /// Pairing worked: keep `token` and sign in with it from now on.
    #[serde(rename_all = "camelCase")]
    Paired {
        v: u32,
        device_id: String,
        name: String,
        token: String,
    },
    #[serde(rename_all = "camelCase")]
    AuthOk {
        v: u32,
        device_id: String,
        name: String,
    },
    State(Box<RemoteState>),
    Pong,
    /// A refused message. Codes: `unauthorized`, `noPairing`, `rateLimited`,
    /// `unsupportedVersion`, `badRequest`, `busy`, `notPressable`, `noDevice`,
    /// `unknownFader`, `unknownPage`, `alreadyAuthenticated`, `timeout`, `tooManyMessages`.
    #[serde(rename_all = "camelCase")]
    Error {
        code: String,
        message: String,
    },
}

impl ServerMessage {
    pub fn error(code: &str, message: impl Into<String>) -> ServerMessage {
        ServerMessage::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Everything the phone draws.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteState {
    pub v: u32,
    /// `None` while no Launchpad (real or virtual) is connected: nothing to show or press.
    pub device: Option<RemoteDeviceInfo>,
    pub layout: Option<Layout>,
    pub pages: Vec<RemotePage>,
    pub active_page: Option<String>,
    pub pads: Vec<RemotePad>,
    pub faders: Vec<RemoteFader>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDeviceInfo {
    pub model: crate::midi::LaunchpadModel,
    pub model_name: String,
    #[serde(rename = "virtual")]
    pub is_virtual: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePage {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePad {
    pub x: u8,
    pub y: u8,
    /// What the LED shows right now: the hardware's own frame.
    pub color: RemoteColor,
    pub pressed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<RemoteLabel>,
    /// The fader this pad belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fader: Option<String>,
}

/// An LED colour with its on-screen RGB (`#rrggbb`), as the desktop shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum RemoteColor {
    Off,
    Palette {
        index: u8,
        rgb: String,
    },
    Flashing {
        index: u8,
        alt: u8,
        rgb: String,
        alt_rgb: String,
    },
    Pulsing {
        index: u8,
        rgb: String,
    },
    Rgb {
        rgb: String,
    },
}

fn hex_rgb(c: crate::midi::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

impl From<LedColor> for RemoteColor {
    fn from(c: LedColor) -> Self {
        match c {
            LedColor::Off => RemoteColor::Off,
            LedColor::Palette(index) => RemoteColor::Palette {
                index,
                rgb: hex_rgb(c.rgb()),
            },
            LedColor::Flashing { index, alt } => RemoteColor::Flashing {
                index,
                alt,
                rgb: hex_rgb(c.rgb()),
                alt_rgb: hex_rgb(palette::palette_color(alt)),
            },
            LedColor::Pulsing(index) => RemoteColor::Pulsing {
                index,
                rgb: hex_rgb(c.rgb()),
            },
            LedColor::Rgb(rgb) => RemoteColor::Rgb { rgb: hex_rgb(rgb) },
        }
    }
}

/// A pad's caption. Image looks are only flagged; their pictures are not sent.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RemoteLabel {
    Text { caption: String, color: String },
    Image,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFader {
    pub id: String,
    pub name: String,
    pub direction: FaderDirection,
    /// Pads from the minimum to the maximum.
    pub pads: Vec<(u8, u8)>,
    pub value: f64,
    /// 0..1 over the travel; what `setFader` takes.
    pub fraction: f64,
    pub display: String,
}

/// What the remote acts through. The app's implementation goes through the device
/// manager and the macro engine, exactly as a press on the hardware does.
pub trait RemoteBackend: Send + Sync {
    fn state(&self) -> RemoteState;
    /// Press or release for a session; `Ok(false)` when refused (held elsewhere,
    /// not pressable, or no longer this session's press).
    fn press(&self, session: u64, x: u8, y: u8, pressed: bool) -> Result<bool, BackendError>;
    fn set_fader(&self, fader_id: &str, fraction: f64) -> Result<(), BackendError>;
    fn switch_page(&self, page_id: &str) -> Result<(), BackendError>;
    /// Ticks whenever the pads may look different.
    fn changes(&self) -> tokio::sync::watch::Receiver<u64>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendError {
    NoDevice,
    UnknownFader,
    UnknownPage,
}

impl BackendError {
    pub fn code(self) -> &'static str {
        match self {
            BackendError::NoDevice => "noDevice",
            BackendError::UnknownFader => "unknownFader",
            BackendError::UnknownPage => "unknownPage",
        }
    }
}

/// The phone's picture of the pads: the hardware's frame plus pages, labels and faders.
pub fn build_state(view: Option<&RenderView>, profile: &Mutex<ProfileStore>) -> RemoteState {
    let Some(view) = view else {
        let store = profile.lock();
        return RemoteState {
            v: PROTOCOL_VERSION,
            device: None,
            layout: None,
            pages: store
                .profile
                .pages
                .iter()
                .map(|p| RemotePage {
                    id: p.id.clone(),
                    name: p.name.clone(),
                })
                .collect(),
            active_page: store.profile.active().map(|p| p.id.clone()),
            pads: Vec::new(),
            faders: Vec::new(),
        };
    };
    build_state_from(&view.device, &view.layout, &view.inputs)
}

pub fn build_state_from(
    device: &ConnectedDevice,
    layout: &Layout,
    inputs: &RenderInputs,
) -> RemoteState {
    let frame = build_frame(layout, inputs);
    let pressed: HashSet<(u8, u8)> = inputs.pressed.lock().clone();
    let live: LiveStatus = inputs.live.lock().clone();
    let store = inputs.profile.lock();
    let page = store.profile.active();
    let pads = layout
        .pads
        .iter()
        .filter(|p| !matches!(p.shape, PadShape::Empty))
        .map(|p| {
            let fader = page
                .and_then(|pg| pg.fader_at(p.x, p.y))
                .map(|(f, _)| f.id.clone());
            let label = page
                .and_then(|pg| pg.get(p.x, p.y))
                .filter(|_| fader.is_none())
                .and_then(|b| match &b.look {
                    Look::Text { caption, color, .. } if !caption.trim().is_empty() => {
                        Some(RemoteLabel::Text {
                            caption: caption.clone(),
                            color: color.clone(),
                        })
                    }
                    Look::Text { .. } => None,
                    Look::Image { .. } => Some(RemoteLabel::Image),
                });
            RemotePad {
                x: p.x,
                y: p.y,
                color: frame
                    .get(&(p.x, p.y))
                    .copied()
                    .unwrap_or(LedColor::Off)
                    .into(),
                pressed: pressed.contains(&(p.x, p.y)),
                label,
                fader,
            }
        })
        .collect();
    let faders = page
        .map(|pg| {
            pg.faders
                .iter()
                .map(|f| {
                    let mut shown = f.clone();
                    if let Some(v) = f.render_value(&live) {
                        shown.value = v;
                    }
                    RemoteFader {
                        id: f.id.clone(),
                        name: f.name.clone(),
                        direction: f.direction,
                        pads: f.pads(),
                        value: shown.value,
                        fraction: shown.fraction(),
                        display: shown.display_text(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    RemoteState {
        v: PROTOCOL_VERSION,
        device: Some(RemoteDeviceInfo {
            model: device.model,
            model_name: device.model_name.clone(),
            is_virtual: device.is_virtual,
        }),
        layout: Some(layout.clone()),
        pages: store
            .profile
            .pages
            .iter()
            .map(|p| RemotePage {
                id: p.id.clone(),
                name: p.name.clone(),
            })
            .collect(),
        active_page: page.map(|p| p.id.clone()),
        pads,
        faders,
    }
}

/// The knob turn a remote fader move becomes: the fader's own first cell on the page
/// on screen, so the engine treats it like a hardware control (level shown at once,
/// the fader's actions once it rests, a bound target set with user origin).
pub fn fader_control(
    profile: &Mutex<ProfileStore>,
    fader_id: &str,
    fraction: f64,
) -> Result<ControlEvent, BackendError> {
    let store = profile.lock();
    let fader = store
        .profile
        .active()
        .and_then(|p| p.faders.iter().find(|f| f.id == fader_id))
        .ok_or(BackendError::UnknownFader)?;
    let value = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    Ok(ControlEvent {
        x: fader.x,
        y: fader.y,
        value,
        kind: ControlKind::Knob,
        released: false,
    })
}

/// A trimmed, printable device name of reasonable length.
pub fn clean_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "Phone".into()
    } else {
        cleaned.to_string()
    }
}
