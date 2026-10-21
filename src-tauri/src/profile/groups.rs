use super::{AudioGroup, AudioMemberTarget, AudioTarget, DeviceRef, Page, Profile};
use std::collections::{HashMap, HashSet};

pub fn validate_groups(groups: &[AudioGroup]) -> Result<(), String> {
    let mut ids = HashSet::new();
    for group in groups {
        if group.id.trim().is_empty() || !ids.insert(&group.id) {
            return Err("audio group IDs must be nonblank and unique".into());
        }
        if group.name.trim().is_empty() || !(1..=32).contains(&group.members.len()) {
            return Err(format!(
                "audio group {} needs a name and 1–32 members",
                group.id
            ));
        }
        let mut members = HashSet::new();
        for member in &group.members {
            let valid = match member {
                AudioMemberTarget::Device {
                    device: DeviceRef::Named { id, .. },
                } => !id.trim().is_empty(),
                AudioMemberTarget::Device { .. } => true,
                AudioMemberTarget::App { app } => !app.id.trim().is_empty(),
                AudioMemberTarget::FineTune { target } => match target {
                    crate::finetune::FineTuneTarget::App { app_id } => !app_id.trim().is_empty(),
                    crate::finetune::FineTuneTarget::Input { device_uid, .. }
                    | crate::finetune::FineTuneTarget::Output { device_uid, .. } => {
                        !device_uid.trim().is_empty()
                    }
                },
                AudioMemberTarget::WaveLink { target } => {
                    use crate::wavelink::WaveLinkTarget::*;
                    match target {
                        Channel { channel_id } => !channel_id.trim().is_empty(),
                        ChannelMix { channel_id, mix_id } => {
                            !channel_id.trim().is_empty() && !mix_id.trim().is_empty()
                        }
                        Mix { mix_id } => !mix_id.trim().is_empty(),
                        Output {
                            output_device_id,
                            output_id,
                        } => !output_device_id.trim().is_empty() && !output_id.trim().is_empty(),
                        Input {
                            input_device_id,
                            input_id,
                        }
                        | InputGain {
                            input_device_id,
                            input_id,
                        } => !input_device_id.trim().is_empty() && !input_id.trim().is_empty(),
                    }
                }
            };
            if !valid || !members.insert(member.identity()) {
                return Err(format!(
                    "audio group {} has an invalid or duplicate member",
                    group.id
                ));
            }
        }
    }
    Ok(())
}

pub fn visit_targets(pages: &mut [Page], mut visit: impl FnMut(&mut AudioTarget, &str)) {
    use crate::macros::ActionKind;
    for page in pages {
        for button in &mut page.buttons {
            let location = format!("page {} button {},{}", page.id, button.x, button.y);
            if let Some(target) = button
                .button
                .state_link
                .as_mut()
                .and_then(|l| l.target.as_mut())
            {
                visit(target, &format!("{location} state link"));
            }
            for (name, list) in [
                ("down", &mut button.button.down),
                ("up", &mut button.button.up),
                ("hold", &mut button.button.hold),
            ] {
                for action in list {
                    if let ActionKind::SetAudio { target, .. } = &mut action.kind {
                        visit(target, &format!("{location} {name} action {}", action.id));
                    }
                }
            }
        }
        for fader in &mut page.faders {
            let location = format!("page {} fader {}", page.id, fader.id);
            if let Some(target) = &mut fader.bound {
                visit(target, &location);
            }
            for (name, list) in [
                ("onChange", &mut fader.on_change),
                ("onTouch", &mut fader.on_touch),
                ("onRelease", &mut fader.on_release),
            ] {
                for action in list {
                    if let ActionKind::SetAudio { target, .. } = &mut action.kind {
                        visit(target, &format!("{location} {name} action {}", action.id));
                    }
                }
            }
        }
    }
}

pub fn references(pages: &[Page]) -> Vec<(String, String)> {
    let mut result = Vec::new();
    visit_targets(&mut pages.to_vec(), |target, location| {
        if let AudioTarget::Group { group_id } = target {
            result.push((group_id.clone(), location.to_string()));
        }
    });
    result
}

pub fn validate_profile(profile: &Profile) -> Result<(), String> {
    validate_groups(&profile.audio_groups)?;
    for (id, location) in references(&profile.pages) {
        if !profile.audio_groups.iter().any(|g| g.id == id) {
            return Err(format!("missing audio group {id} referenced by {location}"));
        }
    }
    Ok(())
}

pub fn dependencies(pages: &[Page], groups: &[AudioGroup]) -> Result<Vec<AudioGroup>, String> {
    let ids: HashSet<_> = references(pages).into_iter().map(|(id, _)| id).collect();
    let result: Vec<_> = groups
        .iter()
        .filter(|g| ids.contains(&g.id))
        .cloned()
        .collect();
    if ids.len() != result.len() {
        return Err("missing audio group export dependency".into());
    }
    Ok(result)
}

/// Validate and remap the entire dependency set before changing the destination.
pub fn import_dependencies(
    profile: &mut Profile,
    pages: &mut [Page],
    groups: Vec<AudioGroup>,
) -> Result<(), String> {
    validate_groups(&groups)?;
    dependencies(pages, &groups)?;
    let mut used: HashSet<_> = profile
        .audio_groups
        .iter()
        .map(|g| g.id.clone())
        .chain(groups.iter().map(|g| g.id.clone()))
        .collect();
    let mut remap = HashMap::new();
    let mut incoming = Vec::new();
    for mut group in groups {
        if let Some(existing) = profile.audio_groups.iter().find(|g| g.id == group.id) {
            if existing == &group {
                continue;
            }
            let new_id = loop {
                let id = uuid::Uuid::new_v4().to_string();
                if used.insert(id.clone()) {
                    break id;
                }
            };
            remap.insert(group.id.clone(), new_id.clone());
            group.id = new_id;
        }
        incoming.push(group);
    }
    visit_targets(pages, |target, _| {
        if let AudioTarget::Group { group_id } = target {
            if let Some(new_id) = remap.get(group_id) {
                *group_id = new_id.clone();
            }
        }
    });
    profile.audio_groups.extend(incoming);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{AudioDeviceKind, Button, Fader};
    use serde_json::json;

    fn member(id: &str, label: &str) -> AudioMemberTarget {
        AudioMemberTarget::Device {
            device: DeviceRef::Named {
                id: id.into(),
                kind: AudioDeviceKind::Output,
                label: label.into(),
            },
        }
    }
    fn group(id: &str) -> AudioGroup {
        AudioGroup {
            id: id.into(),
            name: "Comms".into(),
            members: vec![member("out", "Speakers")],
        }
    }

    #[test]
    fn group_validation_rejects_nesting_names_sizes_and_semantic_duplicates() {
        assert!(serde_json::from_value::<AudioMemberTarget>(
            json!({"type":"group","groupId":"nested"})
        )
        .is_err());
        for size in [0, 33] {
            let mut g = group("g");
            g.members = (0..size).map(|i| member(&i.to_string(), "")).collect();
            assert!(validate_groups(&[g]).is_err());
        }
        let mut g = group("g");
        g.members = (0..32).map(|i| member(&i.to_string(), "")).collect();
        assert!(validate_groups(&[g.clone()]).is_ok());
        g.name = " \n ".into();
        assert!(validate_groups(&[g]).is_err());
        assert!(validate_groups(&[group("g"), group("g")]).is_err());
        let mut g = group("g");
        g.members.push(member("out", "renamed"));
        assert!(validate_groups(&[g]).is_err());
        let mut g = group("g");
        g.members.push(AudioMemberTarget::Device {
            device: DeviceRef::Named {
                id: "out".into(),
                kind: AudioDeviceKind::Input,
                label: "Speakers".into(),
            },
        });
        assert!(
            validate_groups(&[g]).is_ok(),
            "direction is part of identity"
        );
        let mut g = group("g");
        g.members = vec![member("", "label only")];
        assert!(
            validate_groups(&[g]).is_err(),
            "group identity must not depend on labels"
        );
    }

    #[test]
    fn qualified_integration_members_do_not_collide() {
        let mut g = group("g");
        g.members = ["first", "second"]
            .into_iter()
            .map(|parent| AudioMemberTarget::WaveLink {
                target: crate::wavelink::WaveLinkTarget::Output {
                    output_device_id: parent.into(),
                    output_id: "child".into(),
                },
            })
            .collect();
        g.members.push(AudioMemberTarget::WaveLink {
            target: crate::wavelink::WaveLinkTarget::Input {
                input_device_id: "first".into(),
                input_id: "child".into(),
            },
        });
        assert!(validate_groups(&[g]).is_ok());
    }

    #[test]
    fn old_profiles_omit_empty_groups_and_new_profiles_roundtrip() {
        let old = serde_json::to_value(Profile::default()).unwrap();
        assert!(old.get("audioGroups").is_none());
        let mut profile: Profile = serde_json::from_value(old).unwrap();
        profile.audio_groups.push(group("g"));
        profile.pages[0].faders.push(Fader {
            bound: Some(AudioTarget::Group {
                group_id: "g".into(),
            }),
            ..Fader::default()
        });
        let json = serde_json::to_value(&profile).unwrap();
        assert_eq!(
            json["pages"][0]["faders"][0]["bound"],
            json!({"type":"group","groupId":"g"})
        );
        assert_eq!(serde_json::from_value::<Profile>(json).unwrap(), profile);
        profile.audio_groups.clear();
        assert!(serde_json::from_value::<Profile>(serde_json::to_value(profile).unwrap()).is_err());
    }

    #[test]
    fn dependencies_remap_collisions_and_all_reference_locations_atomically() {
        use crate::macros::{Action, ActionKind, SetAudioMode};
        let mut profile = Profile::default();
        profile.audio_groups.push(group("g"));
        let mut incoming = group("g");
        incoming.name = "Other".into();
        let target = AudioTarget::Group {
            group_id: "g".into(),
        };
        let action = Action {
            id: "set".into(),
            wait: true,
            kind: ActionKind::SetAudio {
                target: target.clone(),
                mode: SetAudioMode::SetVolume,
                volume: 30.0,
                volume_from: None,
            },
        };
        let mut page = Page::new("p", "Imported");
        page.faders.push(Fader {
            bound: Some(target),
            on_change: vec![action.clone()],
            on_touch: vec![action.clone()],
            on_release: vec![action.clone()],
            ..Fader::default()
        });
        page.set(
            1,
            2,
            Button {
                down: vec![action.clone()],
                up: vec![action.clone()],
                hold: vec![action],
                ..Button::default()
            },
        );
        let original = profile.clone();
        let original_page = page.clone();
        assert!(
            import_dependencies(&mut profile, std::slice::from_mut(&mut page), vec![]).is_err()
        );
        assert_eq!(profile, original);
        assert_eq!(page, original_page);
        import_dependencies(
            &mut profile,
            std::slice::from_mut(&mut page),
            vec![incoming],
        )
        .unwrap();
        assert_eq!(profile.audio_groups.len(), 2);
        let new_id = &profile.audio_groups[1].id;
        assert_ne!(new_id, "g");
        let refs = references(&[page.clone()]);
        assert_eq!(refs.len(), 7);
        assert!(refs.iter().all(|(id, _)| id == new_id));
        assert_eq!(
            dependencies(&[page], &profile.audio_groups).unwrap(),
            vec![profile.audio_groups[1].clone()]
        );
    }

    #[test]
    fn same_dependency_is_reused_and_invalid_payload_does_not_mutate() {
        let mut profile = Profile::default();
        profile.audio_groups.push(group("g"));
        let original = profile.clone();
        let mut pages = vec![Page::new("p", "p")];
        import_dependencies(&mut profile, &mut pages, vec![group("g")]).unwrap();
        assert_eq!(profile, original);
        let mut invalid = group("bad");
        invalid.members.clear();
        assert!(import_dependencies(&mut profile, &mut pages, vec![invalid]).is_err());
        assert_eq!(profile, original);
    }

    #[test]
    fn profile_mutation_rejects_invalid_group_edits_without_partial_changes() {
        use super::super::{mutate, ProfileStore, SaveKind, SharedProfile};
        use parking_lot::Mutex;
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("lunchpad-group-{}", uuid::Uuid::new_v4()));
        let store: SharedProfile = Arc::new(Mutex::new(ProfileStore::load(&dir)));
        let before = store.lock().profile.clone();
        let result = mutate(&store, SaveKind::Edit, |profile| {
            profile.audio_groups = vec![group("g"), group("g")];
            profile.pages[0].name = "must not leak".into();
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(store.lock().profile, before);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
