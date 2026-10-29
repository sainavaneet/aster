//! `orbit.decisions` (recent journal entries, or a provider key check) and
//! `orbit.route` (a suggested owner role). Provider calls answer from their
//! decision thread; the event loop only gathers inputs.
use std::sync::mpsc::Sender;
use std::sync::Arc;

use serde_json::json;

use super::route::{self, RouteInput, MAX_REQUEST_BYTES};
use super::wall_ms;
use crate::api::schema::{Method, OrbitDecisionsParams, OrbitRouteParams, Request};
use crate::app::api::responses::encode_error;
use crate::app::App;

fn success(id: String, result: serde_json::Value) -> String {
    json!({"id": id, "result": result}).to_string()
}

impl App {
    /// The recent decisions; a key check needs the deferred path.
    pub(crate) fn handle_orbit_decisions(
        &mut self,
        id: String,
        params: OrbitDecisionsParams,
    ) -> String {
        if params.check {
            return encode_error(
                id,
                "deferred_required",
                "The Orbit decision key check requires deferred dispatch",
            );
        }
        let decisions = &self.orbit_decisions;
        success(
            id,
            json!({
                "provider": decisions.provider_name(),
                "decisions": self.orbit_decision_activity(),
            }),
        )
    }

    /// `orbit.route` and `orbit.decisions`; any other method is not handled.
    pub(crate) fn handle_deferred_orbit_decision(
        &mut self,
        request: Request,
        respond_to: Sender<String>,
    ) -> bool {
        let response = match request.method {
            Method::OrbitDecisions(params) if params.check => {
                return self.check_orbit_decision_provider(request.id, respond_to)
            }
            Method::OrbitDecisions(params) => self.handle_orbit_decisions(request.id, params),
            Method::OrbitRoute(params) => match self.route_orbit_request(request.id, params) {
                Ok(job) => return self.spawn_orbit_route(job, respond_to),
                Err(response) => response,
            },
            _ => return false,
        };
        let _ = respond_to.send(response);
        true
    }

    /// `GET /v1/models` with the configured key and endpoint, reporting only
    /// whether it worked.
    fn check_orbit_decision_provider(&mut self, id: String, respond_to: Sender<String>) -> bool {
        let Some(jev) = self.orbit_decisions.jev.clone() else {
            let _ = respond_to.send(encode_error(
                id,
                "orbit_decisions",
                "no provider configured",
            ));
            return true;
        };
        let provider = self.orbit_decisions.provider_name();
        let fallback = respond_to.clone();
        let fallback_id = id.clone();
        let spawned = crate::thread_spawn::spawn_named("aster-orbit-decision-check", move || {
            let mut result = jev.check();
            result["configured_provider"] = json!(provider);
            let _ = respond_to.send(success(id, result));
        });
        if let Err(error) = spawned {
            let _ = fallback.send(encode_error(
                fallback_id,
                "orbit_decisions",
                format!("cannot start the key check: {error}"),
            ));
        }
        true
    }

    fn route_orbit_request(
        &mut self,
        id: String,
        params: OrbitRouteParams,
    ) -> Result<(String, RouteInput), String> {
        let text = params.request.trim();
        if text.is_empty() || text.len() > MAX_REQUEST_BYTES {
            return Err(encode_error(
                id,
                "invalid_params",
                format!("request must be 1 to {MAX_REQUEST_BYTES} bytes"),
            ));
        }
        self.ensure_orbit_team_loaded();
        let roles: Vec<(String, String)> = self
            .orbit_team
            .roster
            .roles
            .iter()
            .map(|role| {
                let area = role.area.trim();
                let area = if area.is_empty() {
                    "not described"
                } else {
                    area
                };
                (role.name.clone(), area.to_owned())
            })
            .collect();
        if roles.is_empty() {
            return Err(encode_error(
                id,
                "orbit_team_empty",
                "No team roles are defined; define them with orbit_team_define",
            ));
        }
        let decisions = &self.orbit_decisions;
        if !decisions.config().routing.enabled || !decisions.has_provider() {
            return Err(success(id, route::disabled(decisions.provider_name())));
        }
        let request = crate::app::orbit_notifications::redact_for_decisions(text);
        Ok((id, RouteInput { request, roles }))
    }

    fn spawn_orbit_route(
        &mut self,
        (id, input): (String, RouteInput),
        respond_to: Sender<String>,
    ) -> bool {
        let config = Arc::clone(&self.orbit_decisions.config);
        let provider = Arc::clone(&self.orbit_decisions.provider);
        let journal = Arc::clone(&self.orbit_decisions.journal);
        let fallback = respond_to.clone();
        let fallback_id = id.clone();
        let spawned = crate::thread_spawn::spawn_named("aster-orbit-route", move || {
            let (result, entry) = route::decide(input, &config, provider.as_ref(), wall_ms());
            journal.append(entry);
            let _ = respond_to.send(success(id, result));
        });
        if let Err(error) = spawned {
            let _ = fallback.send(encode_error(
                fallback_id,
                "orbit_decisions",
                format!("cannot start the routing decision: {error}"),
            ));
        }
        true
    }
}
