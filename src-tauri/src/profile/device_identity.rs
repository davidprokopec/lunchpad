//! Which live device a saved device id means. A device can come back under a
//! different id: macOS gives a USB headset `AppleUSBAudioEngine:…:1` or
//! `LogiGamingAudio:…:1` depending on which driver claimed it. The saved id is
//! never rewritten; callers use the matched live id for writes and readback.
//! Mirrored in `src/lib/deviceIdentity.ts` (shared fixtures in
//! `tools/fixtures/device-identity.json`).

use super::AudioDeviceKind;
use serde::{Deserialize, Serialize};

/// How a saved reference found its live device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceMatch {
    /// The saved id itself
    Exact,
    /// Same CoreAudio UID under another driver prefix
    Driver,
    /// The only device in that direction with the saved name
    Label,
}

/// The id without its driver prefix, for CoreAudio-style
/// `driver:vendor:product:serial:index` UIDs only (at least four segments).
/// Windows endpoint ids (`{0.0.0.00000000}.{guid}`) and short ids never qualify.
fn driverless(id: &str) -> Option<&str> {
    if id.starts_with('{') || id.split(':').count() < 4 {
        return None;
    }
    let (driver, rest) = id.split_once(':')?;
    (!driver.trim().is_empty() && !rest.trim().is_empty()).then_some(rest)
}

/// The live device `saved_id` (or, failing that, `label`) means among `live`
/// `(item, id, name, kind)`: the exact id, else the unique same-direction
/// device with the same UID under another driver, else the unique
/// same-direction device named `label`. Several candidates, or none, is `None`.
pub fn resolve_device<'a, T>(
    saved_id: &str,
    label: Option<&str>,
    kind: AudioDeviceKind,
    live: impl IntoIterator<Item = (T, &'a str, &'a str, AudioDeviceKind)>,
) -> Option<(T, DeviceMatch)> {
    let mut candidates: Vec<(T, &str, &str)> = live
        .into_iter()
        .filter(|(_, _, _, k)| *k == kind)
        .map(|(item, id, name, _)| (item, id, name))
        .collect();
    if let Some(at) = candidates.iter().position(|(_, id, _)| *id == saved_id) {
        return Some((candidates.swap_remove(at).0, DeviceMatch::Exact));
    }
    let unique = |candidates: Vec<(T, &str, &str)>, keep: &dyn Fn(&str, &str) -> bool| {
        let mut found = candidates
            .into_iter()
            .filter(|(_, id, name)| keep(id, name));
        let first = found.next()?;
        found.next().is_none().then_some(first.0)
    };
    if let Some(rest) = driverless(saved_id) {
        let same = |id: &str, _: &str| driverless(id) == Some(rest);
        if candidates.iter().any(|(_, id, name)| same(id, name)) {
            return unique(candidates, &same).map(|item| (item, DeviceMatch::Driver));
        }
    }
    let label = label.map(str::trim).filter(|label| !label.is_empty())?;
    unique(candidates, &|_, name| {
        name.trim().eq_ignore_ascii_case(label)
    })
    .map(|item| (item, DeviceMatch::Label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use AudioDeviceKind::{Input, Output};

    const APPLE_OUT: &str = "AppleUSBAudioEngine:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1";
    const APPLE_IN: &str = "AppleUSBAudioEngine:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:2";
    const LOGI_OUT: &str = "LogiGamingAudio:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1";
    const LOGI_IN: &str = "LogiGamingAudio:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:2";

    fn resolve(
        saved: &str,
        label: Option<&str>,
        kind: AudioDeviceKind,
        live: &[(&'static str, &'static str, AudioDeviceKind)],
    ) -> Option<(&'static str, DeviceMatch)> {
        resolve_device(
            saved,
            label,
            kind,
            live.iter().map(|(id, name, k)| (*id, *id, *name, *k)),
        )
    }

    #[test]
    fn driver_swap_resolves_each_direction_to_its_own_endpoint() {
        let headset = [
            (LOGI_OUT, "PRO X 2", Output),
            (LOGI_IN, "PRO X 2", Input),
            ("BuiltInSpeakerDevice", "MacBook Speakers", Output),
        ];
        assert_eq!(
            resolve(APPLE_OUT, None, Output, &headset),
            Some((LOGI_OUT, DeviceMatch::Driver))
        );
        assert_eq!(
            resolve(APPLE_IN, None, Input, &headset),
            Some((LOGI_IN, DeviceMatch::Driver))
        );
        let apple = [(APPLE_OUT, "PRO X 2", Output), (APPLE_IN, "PRO X 2", Input)];
        assert_eq!(
            resolve(LOGI_OUT, None, Output, &apple),
            Some((APPLE_OUT, DeviceMatch::Driver))
        );
        assert_eq!(
            resolve(APPLE_OUT, None, Output, &apple),
            Some((APPLE_OUT, DeviceMatch::Exact))
        );
        // The :2 input never stands in for a saved :1 output, nor the other way round.
        assert_eq!(resolve(APPLE_OUT, None, Input, &headset), None);
        assert_eq!(
            resolve(APPLE_IN, None, Output, &[(LOGI_IN, "PRO X 2", Input)]),
            None
        );
    }

    #[test]
    fn ambiguous_candidates_stay_unresolved() {
        let both = [
            (LOGI_OUT, "PRO X 2", Output),
            (
                "OtherDriver:Logitech:PRO X 2 LIGHTSPEED:0000000000000000:1",
                "PRO X 2",
                Output,
            ),
        ];
        // Two drivers claim the same remainder: no guessing, and no label fallback either.
        assert_eq!(resolve(APPLE_OUT, Some("PRO X 2"), Output, &both), None);
        let twins = [("a", "USB Mic", Input), ("b", "usb mic", Input)];
        assert_eq!(resolve("gone", Some("USB Mic"), Input, &twins), None);
    }

    #[test]
    fn label_is_the_last_resort_and_needs_one_same_direction_device() {
        let live = [("new", "USB Mic", Input), ("spk", "USB Mic", Output)];
        assert_eq!(
            resolve("old", Some(" usb mic "), Input, &live),
            Some(("new", DeviceMatch::Label))
        );
        assert_eq!(resolve("old", None, Input, &live), None);
        assert_eq!(resolve("old", Some("  "), Input, &live), None);
        assert_eq!(resolve("old", Some("Headset"), Input, &live), None);
    }

    #[test]
    fn only_coreaudio_style_uids_lose_their_driver_prefix() {
        let win = "{0.0.0.00000000}.{11111111-2222-3333-4444-555555555555}";
        let other = "{0.0.0.00000000}.{99999999-2222-3333-4444-555555555555}";
        assert_eq!(
            resolve(win, None, Output, &[(other, "Speakers", Output)]),
            None
        );
        assert_eq!(driverless(win), None);
        assert_eq!(driverless("{0.0.1.00000000}:a:b:c"), None);
        // Bluetooth-style and short ids have too few segments.
        assert_eq!(driverless("00-11-22-33-44-55:output"), None);
        assert_eq!(driverless("a:b:c"), None);
        assert_eq!(driverless(":b:c:d"), None);
        assert_eq!(driverless("a:b:c:d"), Some("b:c:d"));
        assert_eq!(
            resolve("x:b:c:d", None, Output, &[("y:b:c:d", "Y", Output)]),
            Some(("y:b:c:d", DeviceMatch::Driver))
        );
        assert_eq!(
            resolve("x:b:c", None, Output, &[("y:b:c", "Y", Output)]),
            None
        );
    }

    #[test]
    fn shared_fixtures_agree_with_the_ui_mirror() {
        #[derive(Deserialize)]
        struct Live {
            id: String,
            name: String,
            kind: AudioDeviceKind,
        }
        #[derive(Deserialize)]
        struct Saved {
            id: String,
            label: Option<String>,
            kind: AudioDeviceKind,
        }
        #[derive(Deserialize, Debug, PartialEq)]
        struct Expected {
            id: String,
            #[serde(rename = "match")]
            kind: DeviceMatch,
        }
        #[derive(Deserialize)]
        struct Fixture {
            name: String,
            saved: Saved,
            live: Vec<Live>,
            expected: Option<Expected>,
        }
        let fixtures: Vec<Fixture> =
            serde_json::from_str(include_str!("../../../tools/fixtures/device-identity.json"))
                .unwrap();
        assert!(fixtures.len() >= 8);
        for fixture in fixtures {
            let got = resolve_device(
                &fixture.saved.id,
                fixture.saved.label.as_deref(),
                fixture.saved.kind,
                fixture
                    .live
                    .iter()
                    .map(|d| (d.id.as_str(), d.id.as_str(), d.name.as_str(), d.kind)),
            )
            .map(|(id, kind)| Expected {
                id: id.to_string(),
                kind,
            });
            assert_eq!(got, fixture.expected, "{}", fixture.name);
        }
    }
}
