//! Home Assistant through its REST API: `GET /api/states` for the entity list
//! the editors offer, `POST /api/services/<domain>/<service>` to act. A
//! long-lived access token authenticates every call. There is no persistent
//! connection; `refresh` is called at start, when the settings change and when
//! the UI asks, and the result is published as `homeassistant:state`.
//!
//! While the integration is on, a poller reads `GET /api/states/<id>` every
//! [`POLL_INTERVAL`] for the entities a [`HaFollower`] (the macro engine) asks
//! for, and hands their states back so faders can show the real level. It only
//! reads; it backs off on errors and stops when the integration is turned off.

use crate::config::SharedSettings;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

pub const EVENT_HA_STATE: &str = "homeassistant:state";
const TIMEOUT: Duration = Duration::from_secs(8);
/// How often the followed entities are read while all is well.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// The longest wait between reads after errors in a row.
const POLL_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Who wants entity states read back: which entities, and where their states go.
pub trait HaFollower: Send + Sync {
    /// The entity ids worth reading right now (empty = nothing is read).
    fn entities(&self) -> Vec<String>;
    /// The latest state objects (`state` + `attributes`) of those that exist.
    fn follow(&self, states: &HashMap<String, Value>);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HaEntity {
    pub entity_id: String,
    /// `friendly_name`, or the id when there is none
    pub name: String,
    /// The part before the dot: `light`, `switch`, `cover`, …
    pub domain: String,
    pub state: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HaState {
    pub connected: bool,
    pub error: Option<String>,
    pub version: Option<String>,
    pub location: Option<String>,
    pub entities: Vec<HaEntity>,
}

/// Numeric things an entity can be set to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum HaValueKind {
    /// Light brightness, 0–100 %
    #[default]
    Brightness,
    /// Light colour temperature in Kelvin
    ColorTemperature,
    /// Cover position, 0 (closed) – 100 (open)
    Position,
    /// Fan speed, 0–100 %
    FanSpeed,
    /// Media player volume, 0–100 %
    Volume,
    /// `number` / `input_number` value, in the entity's own unit
    Number,
    /// Climate target temperature
    Temperature,
}

/// `domain.object_id` with Home Assistant's characters only: safe to read back
/// (placeholders and typos are not).
pub fn is_entity_id(id: &str) -> bool {
    let ok = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    matches!(id.split_once('.'), Some((domain, object)) if ok(domain) && ok(object))
}

/// What an entity's state object says `kind` is right now, in the unit
/// [`value_service`] writes it in. `None` when it cannot tell (unavailable,
/// unknown, the attribute is missing); a light or fan that is off is at 0.
pub fn read_value(kind: HaValueKind, state: &Value) -> Option<f64> {
    let text = state.get("state").and_then(Value::as_str)?;
    if matches!(text, "unavailable" | "unknown" | "") {
        return None;
    }
    let attr = |name: &str| {
        state
            .get("attributes")
            .and_then(|a| a.get(name))
            .and_then(Value::as_f64)
    };
    match kind {
        HaValueKind::Brightness if text == "off" => Some(0.0),
        // 0–255; rounded the way `brightness_pct` comes back from it.
        HaValueKind::Brightness => {
            attr("brightness").map(|b| (b.clamp(0.0, 255.0) * 100.0 / 255.0).round())
        }
        HaValueKind::ColorTemperature => attr("color_temp_kelvin"),
        HaValueKind::Position => attr("current_position"),
        HaValueKind::FanSpeed if text == "off" => Some(0.0),
        HaValueKind::FanSpeed => attr("percentage"),
        HaValueKind::Volume => attr("volume_level").map(|v| v * 100.0),
        HaValueKind::Number => text.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        HaValueKind::Temperature => attr("temperature"),
    }
}

/// On / off / toggle through the `homeassistant` domain, which works for every entity kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum HaPower {
    #[default]
    On,
    Off,
    Toggle,
}

impl HaPower {
    pub fn service(self) -> &'static str {
        match self {
            HaPower::On => "turn_on",
            HaPower::Off => "turn_off",
            HaPower::Toggle => "toggle",
        }
    }
}

/// The service call that sets `kind` of `entity` to `value`: `(domain, service, data)`.
pub fn value_service(kind: HaValueKind, entity: &str, value: f32) -> (String, &'static str, Value) {
    let pct = || json!(value.clamp(0.0, 100.0).round() as u32);
    match kind {
        HaValueKind::Brightness => ("light".into(), "turn_on", json!({ "brightness_pct": pct() })),
        HaValueKind::ColorTemperature => ("light".into(), "turn_on", json!({ "color_temp_kelvin": value.max(0.0).round() as u32 })),
        HaValueKind::Position => ("cover".into(), "set_cover_position", json!({ "position": pct() })),
        HaValueKind::FanSpeed => ("fan".into(), "set_percentage", json!({ "percentage": pct() })),
        HaValueKind::Volume => ("media_player".into(), "volume_set", json!({ "volume_level": (value.clamp(0.0, 100.0) / 100.0) })),
        HaValueKind::Number => {
            let domain = entity.split('.').next().filter(|d| !d.is_empty()).unwrap_or("input_number").to_string();
            (domain, "set_value", json!({ "value": value }))
        }
        HaValueKind::Temperature => ("climate".into(), "set_temperature", json!({ "temperature": value })),
    }
}

struct Connection {
    url: String,
    token: String,
    client: reqwest::Client,
}

struct Inner {
    settings: SharedSettings,
    app: Option<AppHandle>,
    state: Mutex<HaState>,
    connection: Mutex<Option<Arc<Connection>>>,
    follower: Mutex<Option<Arc<dyn HaFollower>>>,
    /// Stops the running poller.
    poller: Mutex<Option<CancellationToken>>,
    runtime: Handle,
}

#[derive(Clone)]
pub struct HaHandle {
    inner: Arc<Inner>,
}

impl HaHandle {
    /// Create the handle; when the integration is on, the entity list is fetched in the background.
    pub fn spawn(app: Option<AppHandle>, settings: SharedSettings, runtime: &Handle) -> HaHandle {
        let handle = HaHandle {
            inner: Arc::new(Inner {
                settings,
                app,
                state: Mutex::new(HaState::default()),
                connection: Mutex::new(None),
                follower: Mutex::new(None),
                poller: Mutex::new(None),
                runtime: runtime.clone(),
            }),
        };
        let starter = handle.clone();
        runtime.spawn(async move { starter.settings_changed().await });
        handle
    }

    pub fn state(&self) -> HaState {
        self.inner.state.lock().clone()
    }

    fn publish(&self) {
        if let Some(app) = &self.inner.app {
            let _ = app.emit(EVENT_HA_STATE, self.state());
        }
    }

    fn set_state(&self, f: impl FnOnce(&mut HaState)) {
        f(&mut self.inner.state.lock());
        self.publish();
    }

    /// Re-read the settings: fetch the catalog and follow entities when enabled,
    /// forget it all otherwise.
    pub async fn settings_changed(&self) {
        *self.inner.connection.lock() = None;
        let enabled = self.inner.settings.lock().settings.home_assistant.enabled;
        if enabled {
            self.start_polling();
            let _ = self.refresh().await;
        } else {
            self.stop_polling();
            self.set_state(|s| *s = HaState::default());
        }
    }

    /// Who the poller reads entities for; read on every round, so it can be set late.
    pub fn set_follower(&self, follower: Arc<dyn HaFollower>) {
        *self.inner.follower.lock() = Some(follower);
    }

    pub fn polling(&self) -> bool {
        self.inner
            .poller
            .lock()
            .as_ref()
            .is_some_and(|t| !t.is_cancelled())
    }

    fn stop_polling(&self) {
        if let Some(token) = self.inner.poller.lock().take() {
            token.cancel();
        }
    }

    /// (Re)start the poller with a fresh connection; the one before stops.
    fn start_polling(&self) {
        let token = CancellationToken::new();
        if let Some(old) = self.inner.poller.lock().replace(token.clone()) {
            old.cancel();
        }
        let handle = self.clone();
        let fetcher = self.clone();
        let deliverer = self.clone();
        self.inner.runtime.spawn(poll_loop(
            token,
            move || handle.watched(),
            move |ids| {
                let ha = fetcher.clone();
                async move { ha.fetch_states(ids).await }
            },
            move |states| deliverer.deliver(states),
            tokio::time::sleep,
        ));
    }

    /// The entities to read this round: none once the integration is off.
    fn watched(&self) -> Option<Vec<String>> {
        if !self.inner.settings.lock().settings.home_assistant.enabled {
            return None;
        }
        let follower = self.inner.follower.lock().clone();
        let mut ids = follower.map(|f| f.entities()).unwrap_or_default();
        ids.sort();
        ids.dedup();
        Some(ids)
    }

    /// `GET /api/states/<id>` for each; an entity Home Assistant does not know is left out.
    async fn fetch_states(&self, ids: Vec<String>) -> Result<HashMap<String, Value>, String> {
        let conn = self.connection()?;
        let reads = ids.into_iter().map(|id| {
            let conn = conn.clone();
            async move {
                let response = conn
                    .client
                    .get(format!("{}/api/states/{}", conn.url, id))
                    .bearer_auth(&conn.token)
                    .send()
                    .await
                    .map_err(describe)?;
                let status = response.status();
                if status == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                if !status.is_success() {
                    return Err(http_error(
                        status,
                        &response.text().await.unwrap_or_default(),
                    ));
                }
                response
                    .json::<Value>()
                    .await
                    .map(|v| Some((id, v)))
                    .map_err(|e| e.to_string())
            }
        });
        let mut states = HashMap::new();
        for read in futures_util::future::join_all(reads).await {
            if let Some((id, v)) = read? {
                states.insert(id, v);
            }
        }
        Ok(states)
    }

    /// Hand fresh states to the follower and keep the entity list's state strings
    /// current, publishing once and only when one of them changed.
    fn deliver(&self, states: HashMap<String, Value>) {
        let changed = {
            let mut st = self.inner.state.lock();
            sync_entity_states(&mut st.entities, &states)
        };
        if changed {
            self.publish();
        }
        let follower = self.inner.follower.lock().clone();
        if let Some(follower) = follower {
            follower.follow(&states);
        }
    }

    fn connection(&self) -> Result<Arc<Connection>, String> {
        if let Some(c) = self.inner.connection.lock().clone() {
            return Ok(c);
        }
        let cfg = self.inner.settings.lock().settings.home_assistant.clone();
        if !cfg.enabled {
            return Err("Home Assistant is not enabled in the settings".into());
        }
        let url = cfg.url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return Err("no Home Assistant URL".into());
        }
        if cfg.token.trim().is_empty() {
            return Err("no access token".into());
        }
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .danger_accept_invalid_certs(cfg.ignore_tls_errors)
            .build()
            .map_err(|e| e.to_string())?;
        let conn = Arc::new(Connection { url, token: cfg.token.trim().to_string(), client });
        *self.inner.connection.lock() = Some(conn.clone());
        Ok(conn)
    }

    async fn get(&self, conn: &Connection, path: &str) -> Result<Value, String> {
        let response = conn
            .client
            .get(format!("{}{}", conn.url, path))
            .bearer_auth(&conn.token)
            .send()
            .await
            .map_err(|e| describe(e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(http_error(status, &response.text().await.unwrap_or_default()));
        }
        response.json::<Value>().await.map_err(|e| e.to_string())
    }

    /// Fetch the instance facts and every entity; the state event carries the result.
    pub async fn refresh(&self) -> Result<(), String> {
        let result = async {
            let conn = self.connection()?;
            let config = self.get(&conn, "/api/config").await?;
            let states = self.get(&conn, "/api/states").await?;
            let mut entities: Vec<HaEntity> = states
                .as_array()
                .map(|list| list.iter().filter_map(entity_from_state).collect())
                .unwrap_or_default();
            entities.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
            Ok::<_, String>((
                config.get("version").and_then(Value::as_str).map(str::to_string),
                config.get("location_name").and_then(Value::as_str).map(str::to_string),
                entities,
            ))
        }
        .await;
        match result {
            Ok((version, location, entities)) => {
                tracing::info!(count = entities.len(), version = version.as_deref().unwrap_or("?"), "Home Assistant entities loaded");
                self.set_state(|s| {
                    s.connected = true;
                    s.error = None;
                    s.version = version;
                    s.location = location;
                    s.entities = entities;
                });
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = %e, "Home Assistant unreachable");
                self.set_state(|s| {
                    s.connected = false;
                    s.error = Some(e.clone());
                });
                Err(e)
            }
        }
    }

    /// `POST /api/services/<domain>/<service>` with `entity_id` merged into `data`.
    pub async fn call_service(&self, domain: &str, service: &str, entity: Option<&str>, data: Value) -> Result<(), String> {
        let conn = self.connection()?;
        let mut body = match data {
            Value::Object(map) => map,
            Value::Null => serde_json::Map::new(),
            other => return Err(format!("service data must be a JSON object, got {other}")),
        };
        if let Some(id) = entity.map(str::trim).filter(|s| !s.is_empty()) {
            body.insert("entity_id".into(), Value::String(id.to_string()));
        }
        let domain = domain.trim();
        let service = service.trim();
        if domain.is_empty() || service.is_empty() {
            return Err("service needs a domain and a name".into());
        }
        let response = conn
            .client
            .post(format!("{}/api/services/{}/{}", conn.url, domain, service))
            .bearer_auth(&conn.token)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|e| describe(e))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let err = http_error(status, &text);
            self.set_state(|s| s.error = Some(err.clone()));
            return Err(err);
        }
        if !self.inner.state.lock().connected {
            self.set_state(|s| {
                s.connected = true;
                s.error = None;
            });
        }
        Ok(())
    }
}

/// How long to wait before the next read after `failures` errors in a row.
fn poll_delay(failures: u32) -> Duration {
    if failures == 0 {
        return POLL_INTERVAL;
    }
    POLL_INTERVAL
        .saturating_mul(1u32 << failures.min(5))
        .min(POLL_MAX_BACKOFF)
}

/// Read the watched entities, deliver what came back, wait, repeat; until the
/// token is cancelled or `watched` says the integration is off. A round with
/// nothing to watch sends nothing; errors back off up to [`POLL_MAX_BACKOFF`].
async fn poll_loop<W, F, FFut, D, S, SFut>(
    token: CancellationToken,
    watched: W,
    fetch: F,
    deliver: D,
    sleep: S,
) where
    W: Fn() -> Option<Vec<String>>,
    F: Fn(Vec<String>) -> FFut,
    FFut: Future<Output = Result<HashMap<String, Value>, String>>,
    D: Fn(HashMap<String, Value>),
    S: Fn(Duration) -> SFut,
    SFut: Future<Output = ()>,
{
    let mut failures: u32 = 0;
    loop {
        if token.is_cancelled() {
            break;
        }
        let Some(ids) = watched() else { break };
        if !ids.is_empty() {
            let result = tokio::select! {
                _ = token.cancelled() => break,
                r = fetch(ids) => r,
            };
            match result {
                Ok(states) => {
                    if failures > 0 {
                        tracing::info!("Home Assistant states readable again");
                    }
                    failures = 0;
                    deliver(states);
                }
                Err(e) => {
                    if failures == 0 {
                        tracing::warn!(error = %e, "Home Assistant states could not be read, backing off");
                    }
                    failures = failures.saturating_add(1);
                }
            }
        }
        tokio::select! {
            _ = token.cancelled() => break,
            _ = sleep(poll_delay(failures)) => {}
        }
    }
}

/// Copy the `state` strings of `states` into the list; true when one changed.
fn sync_entity_states(entities: &mut [HaEntity], states: &HashMap<String, Value>) -> bool {
    let mut changed = false;
    for entity in entities.iter_mut() {
        let Some(text) = states
            .get(&entity.entity_id)
            .and_then(|v| v.get("state"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if entity.state != text {
            entity.state = text.to_string();
            changed = true;
        }
    }
    changed
}

fn entity_from_state(v: &Value) -> Option<HaEntity> {
    let entity_id = v.get("entity_id")?.as_str()?.to_string();
    let domain = entity_id.split('.').next().unwrap_or("").to_string();
    let name = v
        .get("attributes")
        .and_then(|a| a.get("friendly_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| entity_id.clone());
    let state = v.get("state").and_then(Value::as_str).unwrap_or("").to_string();
    Some(HaEntity { entity_id, name, domain, state })
}

fn describe(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        format!("could not connect: {}", e.without_url())
    } else {
        e.without_url().to_string()
    }
}

fn http_error(status: reqwest::StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(160).collect());
    match status.as_u16() {
        401 => "the access token was rejected".into(),
        404 => format!("not found: {detail}"),
        _ => format!("HTTP {}: {detail}", status.as_u16()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_services() {
        let (d, s, data) = value_service(HaValueKind::Brightness, "light.desk", 42.4);
        assert_eq!((d.as_str(), s), ("light", "turn_on"));
        assert_eq!(data["brightness_pct"], 42);
        let (d, s, data) = value_service(HaValueKind::Volume, "media_player.tv", 55.0);
        assert_eq!((d.as_str(), s), ("media_player", "volume_set"));
        assert!((data["volume_level"].as_f64().unwrap() - 0.55).abs() < 1e-6);
        let (d, _, data) = value_service(HaValueKind::Number, "input_number.gain", -3.5);
        assert_eq!(d, "input_number");
        assert_eq!(data["value"], -3.5);
        let (d, s, _) = value_service(HaValueKind::Number, "number.volume", 7.0);
        assert_eq!((d.as_str(), s), ("number", "set_value"));
        let (_, _, data) = value_service(HaValueKind::Position, "cover.blind", 140.0);
        assert_eq!(data["position"], 100);
        assert_eq!(HaPower::Toggle.service(), "toggle");
    }

    #[test]
    fn entities_from_states() {
        let v = json!({ "entity_id": "light.desk", "state": "on", "attributes": { "friendly_name": "Desk lamp" } });
        let e = entity_from_state(&v).unwrap();
        assert_eq!((e.domain.as_str(), e.name.as_str(), e.state.as_str()), ("light", "Desk lamp", "on"));
        let bare = entity_from_state(&json!({ "entity_id": "switch.plug", "state": "off", "attributes": {} })).unwrap();
        assert_eq!(bare.name, "switch.plug");
    }

    fn st(state: &str, attributes: Value) -> Value {
        json!({ "entity_id": "x.y", "state": state, "attributes": attributes })
    }

    #[test]
    fn values_read_back_per_kind() {
        use HaValueKind::*;
        // Brightness 0–255 comes back as the percentage it was set with.
        assert_eq!(
            read_value(Brightness, &st("on", json!({ "brightness": 128 }))),
            Some(50.0)
        );
        assert_eq!(
            read_value(Brightness, &st("on", json!({ "brightness": 255 }))),
            Some(100.0)
        );
        assert_eq!(
            read_value(Brightness, &st("off", json!({ "brightness": null }))),
            Some(0.0),
            "off is 0"
        );
        assert_eq!(
            read_value(Brightness, &st("unavailable", json!({}))),
            None,
            "unavailable is unknown, not 0"
        );
        assert_eq!(
            read_value(Brightness, &st("unknown", json!({ "brightness": 20 }))),
            None
        );
        assert_eq!(
            read_value(Brightness, &st("on", json!({}))),
            None,
            "on without a brightness is unknown"
        );
        for pct in 0..=100u32 {
            let written = (pct as f64 * 255.0 / 100.0).round();
            assert_eq!(
                read_value(Brightness, &st("on", json!({ "brightness": written }))),
                Some(pct as f64),
                "{pct} % round-trips"
            );
        }
        assert_eq!(
            read_value(
                ColorTemperature,
                &st("on", json!({ "color_temp_kelvin": 2700 }))
            ),
            Some(2700.0)
        );
        assert_eq!(
            read_value(
                ColorTemperature,
                &st("off", json!({ "color_temp_kelvin": null }))
            ),
            None
        );
        assert_eq!(
            read_value(Position, &st("open", json!({ "current_position": 65 }))),
            Some(65.0)
        );
        assert_eq!(
            read_value(Position, &st("closed", json!({ "current_position": 0 }))),
            Some(0.0)
        );
        assert_eq!(
            read_value(FanSpeed, &st("on", json!({ "percentage": 33 }))),
            Some(33.0)
        );
        assert_eq!(
            read_value(FanSpeed, &st("off", json!({ "percentage": 33 }))),
            Some(0.0)
        );
        let v = read_value(Volume, &st("playing", json!({ "volume_level": 0.42 }))).unwrap();
        assert!((v - 42.0).abs() < 1e-9);
        assert_eq!(read_value(Volume, &st("off", json!({}))), None);
        assert_eq!(read_value(Number, &st("-3.5", json!({}))), Some(-3.5));
        assert_eq!(read_value(Number, &st("abc", json!({}))), None);
        assert_eq!(read_value(Number, &st("unavailable", json!({}))), None);
        assert_eq!(
            read_value(Temperature, &st("heat", json!({ "temperature": 21.5 }))),
            Some(21.5)
        );
        assert_eq!(
            read_value(Temperature, &st("off", json!({ "temperature": null }))),
            None
        );
        assert_eq!(
            read_value(Brightness, &json!({ "attributes": {} })),
            None,
            "no state at all"
        );
    }

    #[test]
    fn only_real_entity_ids_are_read() {
        assert!(is_entity_id("light.living_room_2"));
        assert!(!is_entity_id("light.{{room}}"));
        assert!(!is_entity_id("light"));
        assert!(!is_entity_id("light."));
        assert!(!is_entity_id("Light.Desk"));
        assert!(!is_entity_id("light.desk/../x"));
    }

    #[test]
    fn errors_back_off_up_to_a_ceiling() {
        assert_eq!(poll_delay(0), POLL_INTERVAL);
        assert_eq!(poll_delay(1), Duration::from_secs(4));
        assert_eq!(poll_delay(2), Duration::from_secs(8));
        assert_eq!(poll_delay(3), Duration::from_secs(16));
        assert_eq!(poll_delay(4), POLL_MAX_BACKOFF);
        assert_eq!(poll_delay(u32::MAX), POLL_MAX_BACKOFF);
    }

    #[test]
    fn entity_states_update_only_on_change() {
        let mut list = vec![
            HaEntity {
                entity_id: "light.a".into(),
                name: "A".into(),
                domain: "light".into(),
                state: "off".into(),
            },
            HaEntity {
                entity_id: "light.b".into(),
                name: "B".into(),
                domain: "light".into(),
                state: "on".into(),
            },
        ];
        let states: HashMap<String, Value> = [("light.b".to_string(), st("on", json!({})))].into();
        assert!(
            !sync_entity_states(&mut list, &states),
            "the same state publishes nothing"
        );
        let states: HashMap<String, Value> = [
            ("light.a".to_string(), st("on", json!({}))),
            ("light.zz".to_string(), st("on", json!({}))),
        ]
        .into();
        assert!(sync_entity_states(&mut list, &states));
        assert_eq!(list[0].state, "on");
        assert_eq!(list.len(), 2, "unlisted entities are not added");
    }

    /// A scripted poller: what each fetch returns, the waits it asked for, and a
    /// sleep that stops the loop after `rounds` waits.
    struct Script {
        results: Mutex<Vec<Result<HashMap<String, Value>, String>>>,
        fetched: Mutex<Vec<Vec<String>>>,
        delivered: Mutex<Vec<HashMap<String, Value>>>,
        sleeps: Mutex<Vec<Duration>>,
    }

    async fn run_script(script: Arc<Script>, watched: Option<Vec<String>>, rounds: usize) {
        let token = CancellationToken::new();
        let stop = token.clone();
        let (s1, s2, s3) = (script.clone(), script.clone(), script.clone());
        poll_loop(
            token,
            move || watched.clone(),
            move |ids| {
                let s = s1.clone();
                async move {
                    s.fetched.lock().push(ids);
                    let mut results = s.results.lock();
                    if results.is_empty() {
                        Ok(HashMap::new())
                    } else {
                        results.remove(0)
                    }
                }
            },
            move |states| s2.delivered.lock().push(states),
            move |d| {
                let mut sleeps = s3.sleeps.lock();
                sleeps.push(d);
                if sleeps.len() >= rounds {
                    stop.cancel();
                }
                std::future::ready(())
            },
        )
        .await;
    }

    fn script(results: Vec<Result<HashMap<String, Value>, String>>) -> Arc<Script> {
        Arc::new(Script {
            results: Mutex::new(results),
            fetched: Mutex::new(vec![]),
            delivered: Mutex::new(vec![]),
            sleeps: Mutex::new(vec![]),
        })
    }

    #[tokio::test]
    async fn the_poller_backs_off_on_errors_and_recovers() {
        let ok: HashMap<String, Value> = [(
            "light.a".to_string(),
            st("on", json!({ "brightness": 255 })),
        )]
        .into();
        let s = script(vec![
            Err("timed out".into()),
            Err("timed out".into()),
            Ok(ok.clone()),
            Ok(ok),
        ]);
        run_script(s.clone(), Some(vec!["light.a".into()]), 4).await;
        assert_eq!(
            *s.sleeps.lock(),
            vec![
                Duration::from_secs(4),
                Duration::from_secs(8),
                POLL_INTERVAL,
                POLL_INTERVAL
            ]
        );
        assert_eq!(s.fetched.lock().len(), 4);
        assert_eq!(s.delivered.lock().len(), 2, "failed rounds deliver nothing");
    }

    #[tokio::test]
    async fn the_poller_reads_nothing_without_followed_entities() {
        let s = script(vec![]);
        run_script(s.clone(), Some(vec![]), 3).await;
        assert!(
            s.fetched.lock().is_empty(),
            "no request when no fader follows an entity"
        );
        assert_eq!(
            s.sleeps.lock().len(),
            3,
            "it keeps waiting for one to appear"
        );
    }

    #[tokio::test]
    async fn the_poller_stops_once_the_integration_is_off() {
        let s = script(vec![]);
        run_script(s.clone(), None, 100).await;
        assert!(s.fetched.lock().is_empty());
        assert!(s.sleeps.lock().is_empty(), "the loop ended at once");
    }

    #[tokio::test]
    async fn the_poller_runs_only_while_enabled() {
        let dir = std::env::temp_dir().join(format!("lunchpad-ha-{}", uuid::Uuid::new_v4()));
        let settings: SharedSettings =
            Arc::new(Mutex::new(crate::config::SettingsStore::load(&dir)));
        let ha = HaHandle::spawn(None, settings.clone(), &Handle::current());
        ha.settings_changed().await;
        assert!(!ha.polling(), "off by default");
        {
            let mut st = settings.lock();
            st.settings.home_assistant.enabled = true;
            // Nothing listens here; no real Home Assistant is contacted.
            st.settings.home_assistant.url = "http://127.0.0.1:9".into();
            st.settings.home_assistant.token = "test".into();
        }
        ha.settings_changed().await;
        assert!(ha.polling(), "turning it on starts the poller");
        assert!(!ha.state().connected);
        settings.lock().settings.home_assistant.enabled = false;
        ha.settings_changed().await;
        assert!(!ha.polling(), "turning it off stops it");
    }
}
