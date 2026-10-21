//! Platform-independent app-session inventory across every active output
//! endpoint. The Windows backend drives it through [`Platform`]; tests drive it
//! with a fake, so endpoint/session lifecycle, identity and aggregation are
//! checked on any host.
//!
//! * Endpoints: one tracked entry per active render endpoint id, whether or not
//!   it is the default. The list is re-read when the topology is invalidated
//!   (device added, removed, state or default change) and at least every
//!   [`ENDPOINT_CHECK`] otherwise; vanished endpoints are dropped, new ones
//!   opened.
//! * Listing failures: an endpoint whose session listing fails keeps its
//!   sessions, reported with their last-read state, for that one snapshot and
//!   the endpoint list is re-read (a removed endpoint is dropped then). A second
//!   consecutive failure drops the endpoint and its sessions; it is reopened
//!   at the next snapshot. When every open endpoint fails in the same
//!   snapshot, the snapshot is an error so the engine keeps the last published
//!   list (as with the single default endpoint before).
//! * Sessions: keyed by `(endpoint id, session instance id)`, so equal instance
//!   strings on different endpoints never alias. Expired sessions and sessions
//!   no longer listed are dropped. Dropping a platform handle must unregister
//!   its callbacks; sessions always drop before the endpoint that owns them.
//! * Apps: sessions are grouped by app id with [`group`]. When the app has at
//!   least one active session, its volume and mute summarise only the active
//!   sessions (volume is the loudest, muted only when every one is), so a stale
//!   inactive session left on another endpoint (e.g. after a channel move)
//!   cannot mask the audible one. Without an active session every session
//!   counts. `active` is true when any session is; `process_count` counts every
//!   session. Members are ordered by endpoint id, then listing order, so the
//!   display name is deterministic.
//! * Writes: a volume or mute write is sent to every tracked session of the
//!   app on every endpoint, with the same value. A session that disappeared
//!   meanwhile (`NotFound`) is skipped; any other failure does not stop the
//!   remaining sessions. Nothing is rolled back.

use std::time::{Duration, Instant};

use super::engine::{Error, ErrorKind};
use super::model::{group, App, Member};

/// How often the endpoint list is re-read without an invalidation.
pub const ENDPOINT_CHECK: Duration = Duration::from_secs(2);
pub const NO_ENDPOINT: &str = "no active audio output device";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Inactive,
    Active,
    Expired,
}

/// A session as listed by an endpoint, before it is adopted.
pub struct Listed<L> {
    pub instance: String,
    pub state: SessionState,
    pub handle: L,
}

/// A session we hold, attributed to its app, with its callbacks registered.
pub struct Adopted<S> {
    pub app_id: String,
    pub name: String,
    pub session: S,
}

/// The OS side. Every call happens on the thread that owns the inventory.
pub trait Platform {
    /// An opened endpoint with its session-created callback registered;
    /// dropping it unregisters the callback.
    type Endpoint;
    /// An adopted session with its events callback registered; dropping it
    /// unregisters the callback.
    type Session;
    type Listed;

    /// Ids of every active render endpoint.
    fn render_endpoints(&mut self) -> Result<Vec<String>, Error>;
    fn open_endpoint(&mut self, id: &str) -> Result<Self::Endpoint, Error>;
    fn list_sessions(
        &mut self,
        endpoint: &Self::Endpoint,
    ) -> Result<Vec<Listed<Self::Listed>>, Error>;
    /// None when the session cannot be used (it is skipped).
    fn adopt(&mut self, listed: Self::Listed) -> Option<Adopted<Self::Session>>;
    /// Current `(volume, muted)`.
    fn read(&mut self, session: &Self::Session) -> (f32, bool);
    fn set_volume(&mut self, session: &Self::Session, volume: f32) -> Result<(), Error>;
    fn set_mute(&mut self, session: &Self::Session, muted: bool) -> Result<(), Error>;
}

struct Tracked<S> {
    instance: String,
    /// The session's app and its state as last read.
    last: Member,
    session: S,
}

/// Field order is drop order: sessions unregister before their endpoint.
struct Endpoint<P: Platform> {
    id: String,
    /// Consecutive snapshots whose session listing failed.
    failures: u32,
    sessions: Vec<Tracked<P::Session>>,
    endpoint: P::Endpoint,
}

/// Consecutive listing failures after which an endpoint is dropped.
const MAX_LIST_FAILURES: u32 = 2;

pub struct Inventory<P: Platform> {
    /// Sorted by endpoint id.
    endpoints: Vec<Endpoint<P>>,
    topology_due: bool,
    last_check: Option<Instant>,
    last_error: Option<String>,
}

impl<P: Platform> Default for Inventory<P> {
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
            topology_due: true,
            last_check: None,
            last_error: None,
        }
    }
}

enum Write {
    Volume(f32),
    Mute(bool),
}

impl<P: Platform> Inventory<P> {
    /// Re-read the endpoint list at the next snapshot.
    pub fn invalidate(&mut self) {
        self.topology_due = true;
    }

    /// At least one endpoint is open, so apps can be listed and controlled.
    pub fn available(&self) -> bool {
        !self.endpoints.is_empty()
    }

    /// Why apps are unavailable.
    pub fn unavailable_reason(&self) -> String {
        match &self.last_error {
            Some(e) => format!("{NO_ENDPOINT} ({e})"),
            None => NO_ENDPOINT.into(),
        }
    }

    #[cfg(test)]
    fn endpoint_ids(&self) -> Vec<&str> {
        self.endpoints.iter().map(|e| e.id.as_str()).collect()
    }

    /// Opens new endpoints and closes vanished ones when due.
    pub fn sync_endpoints(&mut self, platform: &mut P, now: Instant) {
        let due = self.topology_due
            || self.endpoints.is_empty()
            || self
                .last_check
                .is_none_or(|at| now.saturating_duration_since(at) >= ENDPOINT_CHECK);
        if !due {
            return;
        }
        self.topology_due = false;
        self.last_check = Some(now);
        let mut ids = match platform.render_endpoints() {
            Ok(ids) => ids,
            Err(e) => {
                // Keep what is open: each endpoint's own listing reports failure.
                self.last_error = Some(e.message);
                return;
            }
        };
        ids.sort();
        ids.dedup();
        let mut previous = std::mem::take(&mut self.endpoints);
        let mut error = None;
        for id in ids {
            let endpoint = match previous.iter().position(|e| e.id == id) {
                Some(at) => previous.swap_remove(at),
                None => match platform.open_endpoint(&id) {
                    Ok(endpoint) => Endpoint {
                        id,
                        failures: 0,
                        sessions: Vec::new(),
                        endpoint,
                    },
                    Err(e) => {
                        error.get_or_insert(e.message);
                        continue;
                    }
                },
            };
            self.endpoints.push(endpoint);
        }
        // Endpoints no longer active are dropped here, unregistering them.
        drop(previous);
        self.last_error = error;
    }

    /// The grouped app list across every open endpoint. An error when every
    /// open endpoint failed to list its sessions.
    pub fn snapshot(&mut self, platform: &mut P, now: Instant) -> Result<Vec<App>, Error> {
        self.sync_endpoints(platform, now);
        let mut members = Vec::new();
        let mut failure: Option<Error> = None;
        let mut listed_any = false;
        for endpoint in &mut self.endpoints {
            let listed = match platform.list_sessions(&endpoint.endpoint) {
                Ok(listed) => listed,
                Err(e) => {
                    endpoint.failures += 1;
                    if endpoint.failures < MAX_LIST_FAILURES {
                        members.extend(endpoint.sessions.iter().map(|s| s.last.clone()));
                    }
                    failure.get_or_insert(e);
                    continue;
                }
            };
            listed_any = true;
            endpoint.failures = 0;
            let mut previous = std::mem::take(&mut endpoint.sessions);
            for listed in listed {
                if listed.state == SessionState::Expired
                    || endpoint
                        .sessions
                        .iter()
                        .any(|s| s.instance == listed.instance)
                {
                    continue;
                }
                let mut tracked = match previous.iter().position(|s| s.instance == listed.instance)
                {
                    Some(at) => previous.swap_remove(at),
                    None => match platform.adopt(listed.handle) {
                        Some(adopted) => Tracked {
                            instance: listed.instance,
                            last: Member {
                                app_id: adopted.app_id,
                                name: adopted.name,
                                active: false,
                                volume: 1.0,
                                muted: false,
                            },
                            session: adopted.session,
                        },
                        None => continue,
                    },
                };
                let (volume, muted) = platform.read(&tracked.session);
                tracked.last.active = listed.state == SessionState::Active;
                tracked.last.volume = volume.clamp(0.0, 1.0);
                tracked.last.muted = muted;
                members.push(tracked.last.clone());
                endpoint.sessions.push(tracked);
            }
            // Sessions no longer listed are dropped here, unregistering them.
            drop(previous);
        }
        if let Some(e) = &failure {
            self.last_error = Some(e.message.clone());
            // Re-read the endpoint list: a removed endpoint is dropped then.
            self.topology_due = true;
            // Endpoints failing repeatedly are dropped, unregistering them.
            self.endpoints.retain(|e| e.failures < MAX_LIST_FAILURES);
        }
        match failure {
            Some(e) if !listed_any => Err(e),
            _ => Ok(summarize(members)),
        }
    }

    pub fn set_volume(&mut self, platform: &mut P, app: &str, volume: f32) -> Result<(), Error> {
        self.write(platform, app, Write::Volume(volume))
    }

    pub fn set_mute(&mut self, platform: &mut P, app: &str, muted: bool) -> Result<(), Error> {
        self.write(platform, app, Write::Mute(muted))
    }

    fn write(&mut self, platform: &mut P, app: &str, write: Write) -> Result<(), Error> {
        if self.endpoints.is_empty() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                self.unavailable_reason(),
            ));
        }
        let mut total = 0usize;
        let mut applied = 0usize;
        let mut failure: Option<Error> = None;
        let mut failures = 0usize;
        let sessions = self
            .endpoints
            .iter()
            .flat_map(|e| e.sessions.iter())
            .filter(|s| s.last.app_id == app);
        for tracked in sessions {
            total += 1;
            let result = match write {
                Write::Volume(volume) => platform.set_volume(&tracked.session, volume),
                Write::Mute(muted) => platform.set_mute(&tracked.session, muted),
            };
            match result {
                Ok(()) => applied += 1,
                Err(e) if e.kind == ErrorKind::NotFound => {}
                Err(e) => {
                    failures += 1;
                    failure.get_or_insert(e);
                }
            }
        }
        match failure {
            Some(e) => Err(Error::new(
                e.kind,
                format!(
                    "{failures} of {total} audio sessions failed ({applied} applied): {}",
                    e.message
                ),
            )),
            None if applied == 0 => Err(Error::new(
                ErrorKind::NotFound,
                "the app has no audio session",
            )),
            None => Ok(()),
        }
    }
}

/// [`group`], with volume and mute taken from the active sessions of every
/// app that has one.
fn summarize(members: Vec<Member>) -> Vec<App> {
    let audible: Vec<String> = members
        .iter()
        .filter(|m| m.active)
        .map(|m| m.app_id.clone())
        .collect();
    let counts: Vec<String> = members.iter().map(|m| m.app_id.clone()).collect();
    let mut apps = group(
        members
            .into_iter()
            .filter(|m| m.active || !audible.contains(&m.app_id)),
    );
    for app in &mut apps {
        app.process_count = counts.iter().filter(|id| **id == app.id).count() as u32;
    }
    apps
}

#[cfg(test)]
mod tests {
    use super::super::engine::{
        AppStatus, Backend, BackendKind, Command, Engine, Notifier, Permission,
    };
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug)]
    struct FakeSession {
        instance: String,
        app: String,
        state: SessionState,
        volume: f32,
        muted: bool,
        /// Adopting fails (e.g. the session refuses callback registration).
        unusable: bool,
        /// Writes fail with this kind.
        fail: Option<ErrorKind>,
    }

    fn session(instance: &str, app: &str) -> FakeSession {
        FakeSession {
            instance: instance.into(),
            app: app.into(),
            state: SessionState::Active,
            volume: 1.0,
            muted: false,
            unusable: false,
            fail: None,
        }
    }

    #[derive(Default)]
    struct World {
        /// Active render endpoints and their sessions.
        endpoints: BTreeMap<String, Vec<FakeSession>>,
        enumerate_fails: bool,
        open_fails: Vec<String>,
        list_fails: Vec<String>,
        /// Callbacks currently registered: `endpoint:<id>` and `session:<id>/<inst>`.
        registered: Vec<String>,
        opens: usize,
        adopts: usize,
        writes: Vec<String>,
    }

    impl World {
        fn find(&mut self, endpoint: &str, instance: &str) -> Option<&mut FakeSession> {
            self.endpoints
                .get_mut(endpoint)?
                .iter_mut()
                .find(|s| s.instance == instance)
        }
    }

    type Shared = Arc<Mutex<World>>;

    struct Fake(Shared);

    struct Registration {
        world: Shared,
        name: String,
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            let mut world = self.world.lock().unwrap();
            let at = world
                .registered
                .iter()
                .position(|r| *r == self.name)
                .expect("unregistered exactly once");
            world.registered.remove(at);
        }
    }

    fn register(world: &Shared, name: String) -> Registration {
        world.lock().unwrap().registered.push(name.clone());
        Registration {
            world: Arc::clone(world),
            name,
        }
    }

    struct FakeEndpoint {
        id: String,
        _registration: Registration,
    }

    struct FakeHandle {
        endpoint: String,
        instance: String,
        _registration: Registration,
    }

    impl Platform for Fake {
        type Endpoint = FakeEndpoint;
        type Session = FakeHandle;
        type Listed = (String, String);

        fn render_endpoints(&mut self) -> Result<Vec<String>, Error> {
            let world = self.0.lock().unwrap();
            if world.enumerate_fails {
                return Err(Error::new(ErrorKind::Native, "enumeration failed"));
            }
            Ok(world.endpoints.keys().rev().cloned().collect())
        }
        fn open_endpoint(&mut self, id: &str) -> Result<FakeEndpoint, Error> {
            {
                let mut world = self.0.lock().unwrap();
                if world.open_fails.iter().any(|f| f == id) {
                    return Err(Error::new(ErrorKind::Native, format!("open {id} failed")));
                }
                world.opens += 1;
            }
            Ok(FakeEndpoint {
                id: id.into(),
                _registration: register(&self.0, format!("endpoint:{id}")),
            })
        }
        fn list_sessions(
            &mut self,
            endpoint: &FakeEndpoint,
        ) -> Result<Vec<Listed<(String, String)>>, Error> {
            let world = self.0.lock().unwrap();
            if world.list_fails.contains(&endpoint.id) {
                return Err(Error::new(ErrorKind::NotFound, "device invalidated"));
            }
            let sessions = world
                .endpoints
                .get(&endpoint.id)
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "device invalidated"))?;
            Ok(sessions
                .iter()
                .map(|s| Listed {
                    instance: s.instance.clone(),
                    state: s.state,
                    handle: (endpoint.id.clone(), s.instance.clone()),
                })
                .collect())
        }
        fn adopt(&mut self, (endpoint, instance): (String, String)) -> Option<Adopted<FakeHandle>> {
            let app = {
                let mut world = self.0.lock().unwrap();
                world.adopts += 1;
                let s = world.find(&endpoint, &instance)?;
                if s.unusable {
                    return None;
                }
                s.app.clone()
            };
            let registration = register(&self.0, format!("session:{endpoint}/{instance}"));
            Some(Adopted {
                name: app.trim_end_matches(".exe").into(),
                app_id: app,
                session: FakeHandle {
                    endpoint,
                    instance,
                    _registration: registration,
                },
            })
        }
        fn read(&mut self, session: &FakeHandle) -> (f32, bool) {
            let mut world = self.0.lock().unwrap();
            world
                .find(&session.endpoint, &session.instance)
                .map_or((1.0, false), |s| (s.volume, s.muted))
        }
        fn set_volume(&mut self, session: &FakeHandle, volume: f32) -> Result<(), Error> {
            self.write(session, format!("volume {volume}"), |s| s.volume = volume)
        }
        fn set_mute(&mut self, session: &FakeHandle, muted: bool) -> Result<(), Error> {
            self.write(session, format!("mute {muted}"), |s| s.muted = muted)
        }
    }

    impl Fake {
        fn write(
            &mut self,
            session: &FakeHandle,
            what: String,
            apply: impl FnOnce(&mut FakeSession),
        ) -> Result<(), Error> {
            let mut world = self.0.lock().unwrap();
            world
                .writes
                .push(format!("{}/{} {what}", session.endpoint, session.instance));
            let s = world
                .find(&session.endpoint, &session.instance)
                .filter(|s| s.state != SessionState::Expired)
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "session gone"))?;
            if let Some(kind) = s.fail {
                return Err(Error::new(kind, "write refused"));
            }
            apply(s);
            Ok(())
        }
    }

    const DEFAULT: &str = "{system}";
    const VOICE: &str = "{voice-chat}";
    const GAME: &str = "{game}";

    fn world(endpoints: &[(&str, Vec<FakeSession>)]) -> Shared {
        let world = World {
            endpoints: endpoints
                .iter()
                .map(|(id, s)| (id.to_string(), s.clone()))
                .collect(),
            ..World::default()
        };
        Arc::new(Mutex::new(world))
    }

    struct Rig {
        world: Shared,
        fake: Fake,
        inventory: Inventory<Fake>,
        now: Instant,
    }

    impl Rig {
        fn new(world: Shared) -> Rig {
            Rig {
                fake: Fake(Arc::clone(&world)),
                world,
                inventory: Inventory::default(),
                now: Instant::now(),
            }
        }
        fn try_snapshot(&mut self) -> Result<Vec<App>, Error> {
            self.inventory.snapshot(&mut self.fake, self.now)
        }
        fn snapshot(&mut self) -> Vec<App> {
            self.try_snapshot().expect("snapshot")
        }
        fn app(&mut self, id: &str) -> Option<App> {
            self.snapshot().into_iter().find(|a| a.id == id)
        }
        fn set_volume(&mut self, app: &str, volume: f32) -> Result<(), Error> {
            self.inventory.set_volume(&mut self.fake, app, volume)
        }
        fn set_mute(&mut self, app: &str, muted: bool) -> Result<(), Error> {
            self.inventory.set_mute(&mut self.fake, app, muted)
        }
        fn w(&self) -> std::sync::MutexGuard<'_, World> {
            self.world.lock().unwrap()
        }
        fn registered(&self) -> Vec<String> {
            let mut r = self.w().registered.clone();
            r.sort();
            r
        }
    }

    fn ids(apps: &[App]) -> Vec<&str> {
        apps.iter().map(|a| a.id.as_str()).collect()
    }

    #[test]
    fn apps_on_a_nondefault_endpoint_are_discovered_and_controllable() {
        let mut rig = Rig::new(world(&[
            (DEFAULT, vec![session("s1", "spotify.exe")]),
            (VOICE, vec![session("t1", "teamspeak.exe")]),
        ]));
        let apps = rig.snapshot();
        assert_eq!(ids(&apps), ["spotify.exe", "teamspeak.exe"]);
        assert!(rig.inventory.available());
        assert_eq!(
            rig.inventory.endpoint_ids(),
            [DEFAULT, VOICE],
            "sorted by id"
        );

        rig.set_volume("teamspeak.exe", 0.3).unwrap();
        rig.set_mute("teamspeak.exe", true).unwrap();
        assert_eq!(
            rig.w().writes,
            ["{voice-chat}/t1 volume 0.3", "{voice-chat}/t1 mute true"]
        );
        let ts = rig.app("teamspeak.exe").unwrap();
        assert_eq!((ts.volume, ts.muted), (0.3, true));
        let spotify = rig.app("spotify.exe").unwrap();
        assert_eq!((spotify.volume, spotify.muted), (1.0, false), "independent");
    }

    #[test]
    fn one_app_on_several_endpoints_aggregates_and_writes_every_session() {
        let mut quiet = session("d2", "discord.exe");
        quiet.volume = 0.2;
        quiet.muted = true;
        quiet.state = SessionState::Inactive;
        let mut rig = Rig::new(world(&[
            (VOICE, vec![session("d1", "discord.exe")]),
            (GAME, vec![quiet]),
        ]));
        let discord = rig.app("discord.exe").unwrap();
        assert_eq!(discord.process_count, 2);
        assert_eq!(discord.volume, 1.0, "loudest session");
        assert!(!discord.muted, "muted only when every session is");
        assert!(discord.active, "active when any session is");

        rig.set_volume("discord.exe", 0.5).unwrap();
        rig.set_mute("discord.exe", true).unwrap();
        let mut writes = rig.w().writes.clone();
        writes.sort();
        assert_eq!(
            writes,
            [
                "{game}/d2 mute true",
                "{game}/d2 volume 0.5",
                "{voice-chat}/d1 mute true",
                "{voice-chat}/d1 volume 0.5",
            ]
        );
        let discord = rig.app("discord.exe").unwrap();
        assert_eq!((discord.volume, discord.muted), (0.5, true));
    }

    #[test]
    fn several_sessions_of_one_app_on_one_endpoint_are_one_app() {
        let mut second = session("c2", "chrome.exe");
        second.muted = true;
        let mut rig = Rig::new(world(&[(
            DEFAULT,
            vec![
                session("c1", "chrome.exe"),
                second,
                session("s", "spotify.exe"),
            ],
        )]));
        let apps = rig.snapshot();
        assert_eq!(ids(&apps), ["chrome.exe", "spotify.exe"]);
        assert_eq!(apps[0].process_count, 2);
        rig.set_mute("chrome.exe", false).unwrap();
        assert_eq!(rig.w().writes.len(), 2);
        assert!(!rig.app("chrome.exe").unwrap().muted);
    }

    #[test]
    fn equal_instance_ids_on_different_endpoints_do_not_alias() {
        let mut rig = Rig::new(world(&[
            (VOICE, vec![session("same", "teamspeak.exe")]),
            (GAME, vec![session("same", "game.exe")]),
        ]));
        assert_eq!(ids(&rig.snapshot()), ["game.exe", "teamspeak.exe"]);
        rig.set_volume("teamspeak.exe", 0.4).unwrap();
        assert_eq!(rig.w().writes, ["{voice-chat}/same volume 0.4"]);
        assert_eq!(rig.app("game.exe").unwrap().volume, 1.0);
        // A duplicate instance on one endpoint is tracked once.
        rig.w()
            .endpoints
            .get_mut(VOICE)
            .unwrap()
            .push(session("same", "teamspeak.exe"));
        assert_eq!(rig.app("teamspeak.exe").unwrap().process_count, 1);
        assert_eq!(rig.registered().len(), 4, "two endpoints, two sessions");
    }

    #[test]
    fn sessions_are_adopted_once_and_follow_expiry_and_app_restart() {
        let mut rig = Rig::new(world(&[(VOICE, vec![session("t1", "teamspeak.exe")])]));
        rig.snapshot();
        rig.snapshot();
        assert_eq!(rig.w().adopts, 1, "known sessions are reused");

        rig.w().find(VOICE, "t1").unwrap().state = SessionState::Expired;
        assert!(rig.app("teamspeak.exe").is_none());
        assert_eq!(
            rig.registered(),
            ["endpoint:{voice-chat}"],
            "expired session unregistered"
        );
        assert_eq!(
            rig.set_volume("teamspeak.exe", 0.5).map_err(|e| e.kind),
            Err(ErrorKind::NotFound)
        );

        // The app restarts with a new session on the same endpoint.
        rig.w().endpoints.get_mut(VOICE).unwrap()[0] = session("t2", "teamspeak.exe");
        assert!(rig.app("teamspeak.exe").is_some());
        rig.set_mute("teamspeak.exe", true).unwrap();
        assert_eq!(rig.w().writes, ["{voice-chat}/t2 mute true"]);
        assert_eq!(
            rig.registered(),
            ["endpoint:{voice-chat}", "session:{voice-chat}/t2"]
        );
    }

    #[test]
    fn unusable_sessions_are_skipped_without_hiding_others() {
        let mut odd = session("x", "protected.exe");
        odd.unusable = true;
        let mut rig = Rig::new(world(&[(VOICE, vec![odd, session("t", "teamspeak.exe")])]));
        assert_eq!(ids(&rig.snapshot()), ["teamspeak.exe"]);
        assert!(!rig.registered().iter().any(|r| r.ends_with("/x")));
    }

    #[test]
    fn endpoints_appear_and_disappear_on_invalidation_or_periodic_check() {
        let mut rig = Rig::new(world(&[(DEFAULT, vec![session("s", "spotify.exe")])]));
        rig.snapshot();
        rig.w()
            .endpoints
            .insert(GAME.into(), vec![session("g", "game.exe")]);
        assert_eq!(ids(&rig.snapshot()), ["spotify.exe"], "not due yet");
        rig.inventory.invalidate();
        assert_eq!(ids(&rig.snapshot()), ["game.exe", "spotify.exe"]);

        // Removal noticed by the periodic check even without a notification.
        rig.w().endpoints.remove(GAME);
        rig.now += ENDPOINT_CHECK;
        assert_eq!(ids(&rig.snapshot()), ["spotify.exe"]);
        assert_eq!(
            rig.registered(),
            ["endpoint:{system}", "session:{system}/s"],
            "removed endpoint and its sessions unregistered"
        );
        assert_eq!(rig.w().opens, 2, "surviving endpoint kept open");
    }

    #[test]
    fn a_failing_endpoint_keeps_its_sessions_once_then_is_dropped_and_reopened() {
        let mut ts = session("t", "teamspeak.exe");
        ts.volume = 0.3;
        ts.muted = true;
        let mut rig = Rig::new(world(&[
            (DEFAULT, vec![session("s", "spotify.exe")]),
            (VOICE, vec![ts]),
        ]));
        rig.snapshot();
        rig.w().list_fails.push(VOICE.into());
        rig.w().find(VOICE, "t").unwrap().volume = 0.9;

        // First failure: last-read state kept, endpoint kept, list re-read.
        let kept = rig.app("teamspeak.exe").expect("kept for one failed scan");
        assert_eq!(
            (kept.volume, kept.muted),
            (0.3, true),
            "last read, not a fresh read"
        );
        assert_eq!(rig.inventory.endpoint_ids(), [DEFAULT, VOICE]);
        assert!(rig.registered().iter().any(|r| r.contains("voice")));
        assert!(rig.inventory.topology_due);
        // Writes still reach the kept sessions.
        rig.set_mute("teamspeak.exe", false).unwrap();

        // Second consecutive failure: dropped and unregistered.
        assert_eq!(ids(&rig.snapshot()), ["spotify.exe"]);
        assert_eq!(rig.inventory.endpoint_ids(), [DEFAULT]);
        assert!(!rig.registered().iter().any(|r| r.contains("voice")));

        rig.w().list_fails.clear();
        assert_eq!(ids(&rig.snapshot()), ["spotify.exe", "teamspeak.exe"]);
        assert_eq!(rig.w().opens, 3, "reopened once");
    }

    #[test]
    fn a_recovered_listing_resets_the_failure_count() {
        let mut rig = Rig::new(world(&[
            (DEFAULT, vec![session("s", "spotify.exe")]),
            (VOICE, vec![session("t", "teamspeak.exe")]),
        ]));
        rig.snapshot();
        for _ in 0..3 {
            rig.w().list_fails.push(VOICE.into());
            assert!(rig.app("teamspeak.exe").is_some());
            rig.w().list_fails.clear();
            assert!(rig.app("teamspeak.exe").is_some());
        }
        assert_eq!(rig.w().opens, 2, "intermittent failures never reopen");
    }

    #[test]
    fn a_failing_endpoint_that_was_removed_is_dropped_at_the_reread() {
        let mut rig = Rig::new(world(&[
            (DEFAULT, vec![session("s", "spotify.exe")]),
            (VOICE, vec![session("t", "teamspeak.exe")]),
        ]));
        rig.snapshot();
        rig.w().list_fails.push(VOICE.into());
        rig.snapshot();
        rig.w().endpoints.remove(VOICE);
        assert_eq!(ids(&rig.snapshot()), ["spotify.exe"]);
        assert_eq!(rig.inventory.endpoint_ids(), [DEFAULT]);
    }

    #[test]
    fn when_every_endpoint_fails_the_snapshot_is_an_error() {
        let mut rig = Rig::new(world(&[
            (DEFAULT, vec![session("s", "spotify.exe")]),
            (VOICE, vec![session("t", "teamspeak.exe")]),
        ]));
        rig.snapshot();
        rig.w().list_fails = vec![DEFAULT.into(), VOICE.into()];
        let e = rig.try_snapshot().unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert!(
            rig.inventory.available(),
            "endpoints kept for one failed scan"
        );
        // Persistent failure: still an error (the engine keeps its list), never
        // an empty success, while endpoints keep reopening and failing.
        for _ in 0..4 {
            assert!(rig.try_snapshot().is_err());
        }
        rig.w().list_fails.clear();
        assert_eq!(ids(&rig.snapshot()), ["spotify.exe", "teamspeak.exe"]);
    }

    #[test]
    fn open_failures_and_no_endpoints_report_unavailable_until_recovery() {
        let mut rig = Rig::new(world(&[]));
        assert!(rig.snapshot().is_empty());
        assert!(!rig.inventory.available());
        assert_eq!(rig.inventory.unavailable_reason(), NO_ENDPOINT);
        assert_eq!(
            rig.set_volume("teamspeak.exe", 0.5).map_err(|e| e.kind),
            Err(ErrorKind::Unsupported)
        );

        rig.w()
            .endpoints
            .insert(VOICE.into(), vec![session("t", "teamspeak.exe")]);
        rig.w().open_fails.push(VOICE.into());
        rig.snapshot();
        assert!(!rig.inventory.available());
        assert!(rig
            .inventory
            .unavailable_reason()
            .contains("open {voice-chat} failed"));

        // With nothing open every snapshot retries.
        rig.w().open_fails.clear();
        assert_eq!(ids(&rig.snapshot()), ["teamspeak.exe"]);
        assert!(rig.inventory.available());

        // An enumeration failure keeps what is open.
        rig.w().enumerate_fails = true;
        rig.inventory.invalidate();
        assert_eq!(ids(&rig.snapshot()), ["teamspeak.exe"]);
    }

    /// What a default-device change does on Windows: `DevicesChanged` sets the
    /// sessions flag, `WinBackend::snapshot` turns it into `invalidate()`. That
    /// wiring is Windows-only; this pins the inventory's half of it.
    #[test]
    fn topology_invalidation_reopens_and_readopts_nothing() {
        let mut rig = Rig::new(world(&[
            (DEFAULT, vec![session("s", "spotify.exe")]),
            (VOICE, vec![session("t", "teamspeak.exe")]),
        ]));
        rig.snapshot();
        // A default-device notification only invalidates the topology.
        rig.inventory.invalidate();
        assert_eq!(ids(&rig.snapshot()), ["spotify.exe", "teamspeak.exe"]);
        let w = rig.w();
        assert_eq!(
            (w.opens, w.adopts),
            (2, 2),
            "nothing reopened or re-adopted"
        );
        assert_eq!(w.registered.len(), 4);
    }

    #[test]
    fn external_changes_are_read_back_on_the_next_snapshot() {
        let mut rig = Rig::new(world(&[
            (VOICE, vec![session("t", "teamspeak.exe")]),
            (GAME, vec![session("t", "teamspeak.exe")]),
        ]));
        rig.snapshot();
        {
            let mut w = rig.w();
            let s = w.find(VOICE, "t").unwrap();
            s.volume = 0.25;
            s.muted = true;
        }
        let ts = rig.app("teamspeak.exe").unwrap();
        assert_eq!(
            (ts.volume, ts.muted),
            (1.0, false),
            "GAME session still active, loud and unmuted"
        );
        rig.w().find(GAME, "t").unwrap().muted = true;
        rig.w().find(GAME, "t").unwrap().volume = 0.1;
        let ts = rig.app("teamspeak.exe").unwrap();
        assert_eq!((ts.volume, ts.muted), (0.25, true));
    }

    #[test]
    fn a_stale_inactive_session_does_not_mask_the_active_one() {
        let mut stale = session("old", "teamspeak.exe");
        stale.state = SessionState::Inactive;
        let mut live = session("new", "teamspeak.exe");
        live.volume = 0.2;
        live.muted = true;
        let mut rig = Rig::new(world(&[(DEFAULT, vec![stale]), (VOICE, vec![live])]));
        let ts = rig.app("teamspeak.exe").unwrap();
        assert_eq!((ts.volume, ts.muted, ts.active), (0.2, true, true));
        assert_eq!(ts.process_count, 2, "every session is counted");

        // An external change on the active session is what the app shows.
        rig.w().find(VOICE, "new").unwrap().muted = false;
        rig.w().find(VOICE, "new").unwrap().volume = 0.6;
        let ts = rig.app("teamspeak.exe").unwrap();
        assert_eq!((ts.volume, ts.muted), (0.6, false));
        rig.w().find(DEFAULT, "old").unwrap().muted = true;
        rig.w().find(DEFAULT, "old").unwrap().volume = 0.0;
        assert_eq!(rig.app("teamspeak.exe").unwrap().volume, 0.6);

        // Writes still reach both, so the stale one matches when it resumes.
        rig.set_mute("teamspeak.exe", true).unwrap();
        assert!(rig.w().find(DEFAULT, "old").unwrap().muted);
        assert!(rig.w().find(VOICE, "new").unwrap().muted);
    }

    #[test]
    fn without_an_active_session_every_session_is_summarised() {
        let mut a = session("a", "discord.exe");
        a.state = SessionState::Inactive;
        a.volume = 0.4;
        a.muted = true;
        let mut b = session("b", "discord.exe");
        b.state = SessionState::Inactive;
        b.volume = 0.7;
        let mut rig = Rig::new(world(&[(VOICE, vec![a]), (GAME, vec![b])]));
        let d = rig.app("discord.exe").unwrap();
        assert_eq!(
            (d.volume, d.muted, d.active, d.process_count),
            (0.7, false, false, 2)
        );
        rig.w().find(GAME, "b").unwrap().muted = true;
        assert!(rig.app("discord.exe").unwrap().muted);
    }

    #[test]
    fn partial_write_failures_continue_and_vanished_sessions_are_skipped() {
        let mut refused = session("t2", "teamspeak.exe");
        refused.fail = Some(ErrorKind::Permission);
        let mut rig = Rig::new(world(&[
            (VOICE, vec![session("t1", "teamspeak.exe"), refused]),
            (GAME, vec![session("t3", "teamspeak.exe")]),
        ]));
        rig.snapshot();
        let e = rig.set_volume("teamspeak.exe", 0.6).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Permission);
        assert!(
            e.message
                .starts_with("1 of 3 audio sessions failed (2 applied)"),
            "{}",
            e.message
        );
        assert_eq!(rig.w().writes.len(), 3, "no early stop");
        assert_eq!(rig.w().find(GAME, "t3").unwrap().volume, 0.6);

        // A session that vanished mid-operation is skipped, not an error.
        rig.w().find(VOICE, "t2").unwrap().fail = None;
        rig.w().endpoints.get_mut(GAME).unwrap().clear();
        assert_eq!(rig.set_mute("teamspeak.exe", true), Ok(()));
        // Every session gone: NotFound.
        rig.w().endpoints.get_mut(VOICE).unwrap().clear();
        assert_eq!(
            rig.set_mute("teamspeak.exe", false).map_err(|e| e.kind),
            Err(ErrorKind::NotFound)
        );
        assert!(rig.app("teamspeak.exe").is_none());
    }

    #[test]
    fn dropping_the_inventory_unregisters_sessions_before_endpoints() {
        let world = world(&[
            (DEFAULT, vec![session("s", "spotify.exe")]),
            (VOICE, vec![session("t", "teamspeak.exe")]),
        ]);
        let mut rig = Rig::new(Arc::clone(&world));
        rig.snapshot();
        assert_eq!(rig.registered().len(), 4);
        let Rig { inventory, .. } = rig;
        drop(inventory);
        assert!(world.lock().unwrap().registered.is_empty());
    }

    /// The inventory behind the real engine, as the Windows backend uses it.
    struct InventoryBackend {
        fake: Fake,
        inventory: Inventory<Fake>,
    }

    impl Backend for InventoryBackend {
        fn status(&self) -> AppStatus {
            AppStatus {
                backend: BackendKind::None,
                available: self.inventory.available(),
                can_set_volume: true,
                can_mute: true,
                permission: Permission::Granted,
                message: String::new(),
            }
        }
        fn poll_interval(&self) -> Duration {
            // Only notifications and commands refresh.
            Duration::from_secs(3600)
        }
        fn snapshot(&mut self) -> Result<Vec<App>, Error> {
            self.inventory.snapshot(&mut self.fake, Instant::now())
        }
        fn set_volume(&mut self, app: &str, volume: f32) -> Result<(), Error> {
            self.inventory.set_volume(&mut self.fake, app, volume)
        }
        fn set_mute(&mut self, app: &str, muted: bool) -> Result<(), Error> {
            self.inventory.set_mute(&mut self.fake, app, muted)
        }
    }

    #[test]
    fn engine_publishes_writes_and_notified_external_changes_across_endpoints() {
        const WAIT: Duration = Duration::from_secs(2);
        let shared = world(&[
            (DEFAULT, vec![session("s", "spotify.exe")]),
            (VOICE, vec![session("t", "teamspeak.exe")]),
        ]);
        let (tx, rx) = std::sync::mpsc::channel::<Notifier>();
        let fake = Fake(Arc::clone(&shared));
        let engine = Engine::start(
            BackendKind::None,
            Box::new(move |notifier| {
                tx.send(notifier).unwrap();
                Ok(Box::new(InventoryBackend {
                    fake,
                    inventory: Inventory::default(),
                }))
            }),
        );
        let notifier = rx.recv_timeout(WAIT).unwrap();
        let find = |id: &str| engine.list().unwrap().into_iter().find(|a| a.id == id);
        assert!(
            find("teamspeak.exe").is_some(),
            "nondefault endpoint listed"
        );
        let sub = engine.subscribe();

        engine
            .command(Command::AppVolume("teamspeak.exe".into(), 0.4))
            .unwrap();
        assert!(sub.recv_timeout(WAIT).unwrap().apps());
        assert_eq!(find("teamspeak.exe").unwrap().volume, 0.4, "readback");
        assert_eq!(shared.lock().unwrap().find(VOICE, "t").unwrap().volume, 0.4);

        // External change (another mixer) plus its session callback.
        shared.lock().unwrap().find(VOICE, "t").unwrap().muted = true;
        notifier.notify();
        assert!(sub.recv_timeout(WAIT).unwrap().apps());
        assert!(find("teamspeak.exe").unwrap().muted);
        assert!(!find("spotify.exe").unwrap().muted);

        // The session expires: the app disappears and writes are refused.
        shared.lock().unwrap().find(VOICE, "t").unwrap().state = SessionState::Expired;
        notifier.notify();
        assert!(sub.recv_timeout(WAIT).unwrap().apps());
        assert!(find("teamspeak.exe").is_none());
        assert_eq!(
            engine
                .command(Command::AppMute("teamspeak.exe".into(), false))
                .map_err(|e| e.kind),
            Err(ErrorKind::NotFound)
        );
        engine.shutdown();
        assert!(
            shared.lock().unwrap().registered.is_empty(),
            "worker dropped everything"
        );
    }

    #[test]
    fn sessions_unregister_before_their_endpoint() {
        let order = Arc::new(Mutex::new(Vec::new()));
        struct Logged(Arc<Mutex<Vec<&'static str>>>, &'static str);
        impl Drop for Logged {
            fn drop(&mut self) {
                self.0.lock().unwrap().push(self.1);
            }
        }
        let endpoint: Endpoint<OrderOnly> = Endpoint {
            id: "e".into(),
            failures: 0,
            sessions: vec![Tracked {
                instance: "i".into(),
                last: Member {
                    app_id: "a".into(),
                    name: "a".into(),
                    active: false,
                    volume: 1.0,
                    muted: false,
                },
                session: Logged(Arc::clone(&order), "session"),
            }],
            endpoint: Logged(Arc::clone(&order), "endpoint"),
        };
        drop(endpoint);
        assert_eq!(*order.lock().unwrap(), ["session", "endpoint"]);

        struct OrderOnly;
        impl Platform for OrderOnly {
            type Endpoint = Logged;
            type Session = Logged;
            type Listed = ();
            fn render_endpoints(&mut self) -> Result<Vec<String>, Error> {
                unreachable!()
            }
            fn open_endpoint(&mut self, _: &str) -> Result<Logged, Error> {
                unreachable!()
            }
            fn list_sessions(&mut self, _: &Logged) -> Result<Vec<Listed<()>>, Error> {
                unreachable!()
            }
            fn adopt(&mut self, _: ()) -> Option<Adopted<Logged>> {
                unreachable!()
            }
            fn read(&mut self, _: &Logged) -> (f32, bool) {
                unreachable!()
            }
            fn set_volume(&mut self, _: &Logged, _: f32) -> Result<(), Error> {
                unreachable!()
            }
            fn set_mute(&mut self, _: &Logged, _: bool) -> Result<(), Error> {
                unreachable!()
            }
        }
    }
}
