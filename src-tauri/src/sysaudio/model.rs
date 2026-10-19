//! Platform-independent model: apps, devices, grouping and id rules. Pure, so
//! it is tested on any host.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Longest app id, in bytes.
pub const MAX_ID_LEN: usize = 128;

/// One application: every process or session of it, grouped under one stable id.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct App {
    pub id: String,
    pub name: String,
    /// 0..1
    pub volume: f32,
    pub muted: bool,
    /// Producing audio right now.
    pub active: bool,
    pub process_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceKind {
    Output,
    Input,
}

/// One endpoint in one direction. A duplex device appears twice, once per
/// kind; `(id, kind)` is the key.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    /// Opaque OS identifier (Core Audio UID, Windows endpoint id). Never
    /// shortened or rewritten: that could address a different endpoint.
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    /// 0..1
    pub volume: f32,
    pub muted: bool,
    pub is_default: bool,
    pub can_set_volume: bool,
    pub can_mute: bool,
}

pub fn valid_device_id(id: &str) -> bool {
    !id.is_empty() && id.chars().count() <= 256 && !id.chars().any(char::is_control)
}

/// One audio-producing process or session, already attributed to its app.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub app_id: String,
    pub name: String,
    pub active: bool,
    pub volume: f32,
    pub muted: bool,
}

/// Maps any string onto the `[A-Za-z0-9._:-]{1,128}` id alphabet. Characters
/// outside it become `-`; the result is cut to 128 bytes; empty input gives
/// `unknown`.
pub fn slugify(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Every char is ASCII now, so any byte index is a char boundary.
    out.truncate(MAX_ID_LEN);
    if out.is_empty() {
        out.push_str("unknown");
    }
    out
}

pub fn is_valid_id(id: &str) -> bool {
    (1..=MAX_ID_LEN).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// Groups members by app id. Volume is the loudest member, `muted` only when
/// every member is muted, `active` when any member is. The name comes from the
/// first member. Sorted by case-insensitive name, then id, for a stable order.
pub fn group(members: impl IntoIterator<Item = Member>) -> Vec<App> {
    let mut apps: Vec<App> = Vec::new();
    for m in members {
        match apps.iter_mut().find(|a| a.id == m.app_id) {
            Some(app) => {
                app.volume = app.volume.max(m.volume);
                app.muted &= m.muted;
                app.active |= m.active;
                app.process_count += 1;
            }
            None => apps.push(App {
                id: m.app_id,
                name: m.name,
                volume: m.volume,
                muted: m.muted,
                active: m.active,
                process_count: 1,
            }),
        }
    }
    apps.sort_by(compare_apps);
    apps
}

fn compare_apps(a: &App, b: &App) -> Ordering {
    a.name
        .to_lowercase()
        .cmp(&b.name.to_lowercase())
        .then_with(|| a.id.cmp(&b.id))
}

/// Last path component, accepting both `/` and `\` separators.
pub fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Windows grouping: the app id is the lower-cased executable file name.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn windows_app_id(image_path: &str) -> String {
    slugify(&basename(image_path).to_lowercase())
}

/// Windows display name: the executable file name without `.exe`.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn windows_app_name(image_path: &str) -> String {
    let file = basename(image_path);
    match file.len().checked_sub(4) {
        Some(cut) if file.is_char_boundary(cut) && file[cut..].eq_ignore_ascii_case(".exe") => {
            file[..cut].to_string()
        }
        _ => file.to_string(),
    }
}

/// macOS grouping: the outermost `.app` bundle on an executable path, so
/// helpers nested inside an app (`Chrome.app/.../Google Chrome Helper.app/...`)
/// group under the app itself.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn outermost_app(path: &str) -> Option<&str> {
    let mut offset = 0;
    for component in path.split('/') {
        offset += component.len();
        if component.len() > 4 && component.ends_with(".app") {
            return Some(&path[..offset]);
        }
        offset += 1;
    }
    None
}

/// Display name of an `.app` bundle path: its folder name without `.app`.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn app_bundle_name(bundle_path: &str) -> String {
    let file = basename(bundle_path);
    file.strip_suffix(".app").unwrap_or(file).to_string()
}

/// XPC services (e.g. Safari's `com.apple.WebKit.GPU`) produce audio on
/// behalf of the app responsible for them.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn is_xpc_service(path: &str) -> bool {
    path.contains(".xpc/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_are_opaque_bounded_unicode_not_app_slugs() {
        assert!(valid_device_id("{endpoint} / é"));
        assert!(valid_device_id(&"🦀".repeat(256)));
        assert!(!valid_device_id(&"🦀".repeat(257)));
        assert!(!valid_device_id(""));
        assert!(!valid_device_id("bad\0id"));
        assert!(!valid_device_id("bad\nid"));
    }

    fn member(id: &str, volume: f32, muted: bool, active: bool) -> Member {
        Member {
            app_id: id.into(),
            name: id.into(),
            active,
            volume,
            muted,
        }
    }

    #[test]
    fn slugify_keeps_valid_ids_and_replaces_the_rest() {
        assert_eq!(slugify("com.apple.Music"), "com.apple.Music");
        assert_eq!(slugify("proc:afplay"), "proc:afplay");
        assert_eq!(slugify("My App (beta).exe"), "My-App--beta-.exe");
        assert_eq!(slugify("hudba/ž"), "hudba--");
        assert_eq!(slugify(""), "unknown");
        let long = slugify(&"é".repeat(200));
        assert_eq!(long.len(), MAX_ID_LEN);
        assert!(is_valid_id(&long));
    }

    #[test]
    fn slug_output_is_always_a_valid_id() {
        for raw in ["", " ", "a b", "C:\\x\\y.exe", "日本語", &"x".repeat(500)] {
            assert!(is_valid_id(&slugify(raw)), "{raw:?}");
        }
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("a b"));
        assert!(!is_valid_id(&"a".repeat(129)));
    }

    #[test]
    fn group_merges_members_of_one_app() {
        let apps = group([
            member("com.google.Chrome", 0.5, true, false),
            member("com.spotify.client", 0.8, false, true),
            member("com.google.Chrome", 0.9, false, true),
        ]);
        assert_eq!(apps.len(), 2);
        let chrome = &apps[0];
        assert_eq!(chrome.id, "com.google.Chrome");
        assert_eq!(chrome.volume, 0.9);
        assert!(!chrome.muted, "muted only when every member is muted");
        assert!(chrome.active);
        assert_eq!(chrome.process_count, 2);
        assert_eq!(apps[1].process_count, 1);
    }

    #[test]
    fn group_is_muted_when_all_members_are() {
        let apps = group([member("a", 0.2, true, false), member("a", 0.4, true, false)]);
        assert!(apps[0].muted);
        assert_eq!(apps[0].volume, 0.4);
    }

    #[test]
    fn group_sorts_by_name_case_insensitively() {
        let apps = group([
            Member {
                app_id: "z".into(),
                name: "beta".into(),
                active: false,
                volume: 1.0,
                muted: false,
            },
            Member {
                app_id: "y".into(),
                name: "Alpha".into(),
                active: false,
                volume: 1.0,
                muted: false,
            },
        ]);
        assert_eq!(
            apps.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["y", "z"]
        );
    }

    #[test]
    fn windows_grouping_uses_the_executable_name() {
        let path = r"C:\Program Files\Spotify\Spotify.exe";
        assert_eq!(windows_app_id(path), "spotify.exe");
        assert_eq!(windows_app_name(path), "Spotify");
        assert_eq!(windows_app_id(r"C:\Apps\My Player.EXE"), "my-player.exe");
        assert_eq!(windows_app_name(r"C:\Apps\My Player.EXE"), "My Player");
        assert_eq!(windows_app_name("tool"), "tool");
        assert_eq!(
            windows_app_id(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            windows_app_id(r"C:\Other\CHROME.exe"),
        );
    }

    #[test]
    fn macos_grouping_uses_the_outermost_app_bundle() {
        let helper = "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/131/Helpers/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper";
        assert_eq!(
            outermost_app(helper),
            Some("/Applications/Google Chrome.app")
        );
        assert_eq!(
            app_bundle_name("/Applications/Google Chrome.app"),
            "Google Chrome"
        );
        assert_eq!(
            outermost_app("/System/Applications/Music.app/Contents/MacOS/Music"),
            Some("/System/Applications/Music.app")
        );
        assert_eq!(outermost_app("/usr/bin/afplay"), None);
        assert_eq!(
            outermost_app("/x/.app/y"),
            None,
            "a bare .app component is not a bundle"
        );
        let gpu = "/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.GPU.xpc/Contents/MacOS/com.apple.WebKit.GPU";
        assert_eq!(outermost_app(gpu), None);
        assert!(is_xpc_service(gpu));
        assert!(!is_xpc_service(helper));
    }

    #[test]
    fn devices_and_apps_serialise_for_the_window() {
        let device = Device {
            id: "uid".into(),
            name: "Mic".into(),
            kind: DeviceKind::Input,
            volume: 0.5,
            muted: true,
            is_default: true,
            can_set_volume: true,
            can_mute: true,
        };
        assert_eq!(
            serde_json::to_value(&device).unwrap(),
            serde_json::json!({"id":"uid","name":"Mic","kind":"input","volume":0.5,"muted":true,"isDefault":true,"canSetVolume":true,"canMute":true})
        );
        let app = App {
            id: "a".into(),
            name: "A".into(),
            volume: 1.0,
            muted: false,
            active: true,
            process_count: 2,
        };
        assert_eq!(serde_json::to_value(&app).unwrap()["processCount"], 2);
    }
}
