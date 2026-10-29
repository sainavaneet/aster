//! Lead binding: persisted terminal id first, `polaris` name as the fallback.
use super::persistence::{restart, saved, with_store};
use super::*;
use crate::app::orbit_notifications::store;

fn rename(app: &mut App, terminal_id: &str, name: &str) {
    app.state
        .terminals
        .get_mut(terminal_id)
        .unwrap()
        .set_agent_name(name.into());
}

#[tokio::test]
async fn renamed_lead_still_receives_assignments() {
    let (mut app, mut worker, _lead) = setup();
    let first = assign(&mut app).await;
    drain(&mut worker);
    assert!(report(&mut app, first.clone()).get("error").is_none());
    let lead = lead_id(&app);
    rename(&mut app, &lead, "chief");
    let second = assign(&mut app).await;
    assert_eq!(task(&app, &second.task_id).lead_id, lead);
    assert_eq!(task(&app, &first.task_id).lead_id, lead);
}

#[tokio::test]
async fn lead_binding_survives_ledger_reload() {
    let path = store::test_path("lead-binding");
    let (mut before, mut worker, _lead) = setup();
    with_store(&mut before, &path);
    let first = assign(&mut before).await;
    drain(&mut worker);
    assert!(report(&mut before, first).get("error").is_none());
    let lead = lead_id(&before);
    assert_eq!(saved(&path)["lead_terminal_id"], lead.as_str());
    let mut app = restart(&mut before, &path);
    rename(&mut app, &lead, "chief");
    let response = try_assign(&mut app, None);
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["lead_id"], lead.as_str());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn missing_bound_terminal_falls_back_to_the_polaris_name() {
    let (mut app, _worker, _lead) = setup();
    app.orbit_notifications.lead_terminal_id = Some("gone-terminal".into());
    let result = assign(&mut app).await;
    let lead = app.resolve_agent_target("polaris").unwrap().terminal_id;
    assert_eq!(task(&app, &result.task_id).lead_id, lead);
    assert_eq!(lead_id(&app), lead, "binding re-recorded");
}

#[tokio::test]
async fn activity_reports_lead_status_without_secrets() {
    let (mut app, _worker, _lead) = setup();
    let result = assign(&mut app).await;
    assert!(report(&mut app, result.clone()).get("error").is_none());
    let activity = api(&mut app, "orbit.activity", json!({}));
    let lead = &activity["result"]["lead"];
    assert_eq!(lead["terminal_id"], lead_id(&app).as_str(), "{activity}");
    assert_eq!(lead["present"], true);
    assert_eq!(lead["state"], "idle");
    assert_eq!(lead["visible_idle"], true);
    assert_eq!(lead["visible_working"], false);
    assert_eq!(lead["queued_reports"], 1);
    assert!(activity["result"]["lead_error"].is_null());
    let text = activity.to_string();
    assert!(!text.contains("token_hash"), "{text}");
    assert!(!text.contains(result.token.as_deref().unwrap()));
}

#[tokio::test]
async fn activity_reports_a_visible_working_lead() {
    let (mut app, _worker, _lead) = setup();
    let lead = app.resolve_agent_target("polaris").unwrap().terminal_id;
    app.state
        .terminals
        .get_mut(lead.as_str())
        .unwrap()
        .set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Working,
            false,
            false,
            true,
            false,
            Instant::now(),
        );
    let activity = api(&mut app, "orbit.activity", json!({}));
    let lead = &activity["result"]["lead"];
    assert_eq!(lead["state"], "working", "{activity}");
    assert_eq!(lead["visible_working"], true, "{activity}");
    assert_eq!(lead["visible_idle"], false);
}

/// After a restart a second agent named `polaris` appears: the bound lead
/// stays the one lead for delivery and approvals; only with no binding are
/// both treated as the lead, so neither is ever managed as a worker.
#[tokio::test]
async fn after_a_restart_a_second_polaris_never_displaces_the_bound_lead() {
    let path = store::test_path("two-leads");
    let (mut before, mut worker, _lead) = setup();
    with_store(&mut before, &path);
    let first = assign(&mut before).await;
    drain(&mut worker);
    let lead = lead_id(&before);
    let mut app = restart(&mut before, &path);
    rename(&mut app, &lead, "chief");
    let impostor = first.worker_id.clone();
    rename(&mut app, &impostor, "polaris");
    // The server loads the ledger, and with it the binding, on its first tick.
    pump(&mut app);
    assert_eq!(app.orbit_lead_terminal().unwrap().terminal_id, lead);
    assert!(app.orbit_is_lead_terminal(&lead));
    assert!(!app.orbit_is_lead_terminal(&impostor));
    assert!(report(&mut app, first.clone()).get("error").is_none());
    pump_until(&mut app, &first.task_id, Delivery::Submitted).await;
    assert_eq!(task(&app, &first.task_id).lead_id, lead);
    // No binding and two agents named polaris: no unique lead.
    rename(&mut app, &lead, "polaris");
    app.orbit_notifications.lead_terminal_id = None;
    assert!(app.orbit_lead_terminal().is_none());
    assert!(app.orbit_is_lead_terminal(&lead) && app.orbit_is_lead_terminal(&impostor));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
