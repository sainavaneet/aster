//! Human notifications: one per task and event kind, filtered by
//! `[polaris.notify] events`, and none while `ui.toast.delivery` is `off`.
use super::super::lifecycle::STARTUP_GRACE_MS;
use super::super::pump::MAX_ATTEMPTS;
use super::*;
use crate::config::{OrbitNotifyEvent, ToastDelivery};

fn deliver_toasts(app: &mut App, delivery: ToastDelivery) {
    app.state.toast_config.delivery = delivery;
}

fn notify_only(app: &mut App, events: &[OrbitNotifyEvent]) {
    let mut config = crate::config::PolarisConfig::default();
    config.notify.events = events.to_vec();
    app.configure_orbit_notifications(&config);
}

fn kinds(app: &mut App) -> Vec<OrbitNotifyEvent> {
    app.take_orbit_human_notices()
        .into_iter()
        .map(|notice| notice.event)
        .collect()
}

#[tokio::test]
async fn delivered_report_notifies_once_without_summary() {
    let (mut app, _worker, _lead) = setup();
    deliver_toasts(&mut app, ToastDelivery::Aster);
    let result = assign(&mut app).await;
    assert!(report(&mut app, result.clone()).get("error").is_none());
    pump_until(&mut app, &result.task_id, Delivery::Submitted).await;
    let notices = app.take_orbit_human_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let notice = &notices[0];
    assert_eq!(notice.event, OrbitNotifyEvent::Report);
    assert_eq!(notice.title, "Orbit report from reviewer");
    assert!(notice.body.contains(&result.task_id), "{}", notice.body);
    assert!(!notice.body.contains("Review complete"));
    assert!(notice.pane_id.is_some());
    // Identical retries and later ticks never notify again.
    assert!(report(&mut app, result).get("error").is_none());
    pump(&mut app);
    pump(&mut app);
    assert!(kinds(&mut app).is_empty());
}

#[tokio::test]
async fn delivery_off_raises_nothing() {
    let (mut app, _worker, _lead) = setup();
    deliver_toasts(&mut app, ToastDelivery::Off);
    let result = assign(&mut app).await;
    assert!(report(&mut app, result.clone()).get("error").is_none());
    pump_until(&mut app, &result.task_id, Delivery::Submitted).await;
    pump(&mut app);
    assert!(kinds(&mut app).is_empty());
}

#[tokio::test]
async fn events_list_filters_and_failed_delivery_notifies_once() {
    let (mut app, _worker, _lead) = setup();
    deliver_toasts(&mut app, ToastDelivery::System);
    notify_only(&mut app, &[OrbitNotifyEvent::Failed]);
    let delivered = assign(&mut app).await;
    assert!(report(&mut app, delivered.clone()).get("error").is_none());
    pump_until(&mut app, &delivered.task_id, Delivery::Submitted).await;
    assert!(kinds(&mut app).is_empty(), "report is filtered out");
    let failing = assign(&mut app).await;
    assert!(report(&mut app, failing.clone()).get("error").is_none());
    task_mut(&mut app, &failing.task_id).attempts = MAX_ATTEMPTS;
    pump(&mut app);
    assert_eq!(task(&app, &failing.task_id).delivery, Delivery::Failed);
    let notices = app.take_orbit_human_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].event, OrbitNotifyEvent::Failed);
    assert_eq!(notices[0].title, "Orbit delivery failed");
    assert!(notices[0].body.contains(&failing.task_id));
    assert!(notices[0].body.contains("reviewer"));
    pump(&mut app);
    assert!(kinds(&mut app).is_empty());
}

#[tokio::test]
async fn server_notice_notifies_once_and_its_delivery_is_not_a_report() {
    let (mut app, _worker, _lead) = setup();
    deliver_toasts(&mut app, ToastDelivery::Terminal);
    let result = assign(&mut app).await;
    let key = app
        .state
        .terminals
        .get_key_value(result.worker_id.as_str())
        .unwrap()
        .0
        .clone();
    app.terminal_runtimes.remove(&key);
    pump_at(
        &mut app,
        Instant::now() + Duration::from_millis(STARTUP_GRACE_MS + 1_000),
    );
    assert_eq!(task(&app, &result.task_id).delivery, Delivery::WorkerLost);
    let notices = app.take_orbit_human_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].event, OrbitNotifyEvent::Notice);
    assert!(
        notices[0].body.contains("worker lost"),
        "{}",
        notices[0].body
    );
    assert!(notices[0].body.contains(&result.task_id));
    let notice = format!("{}/server-notice", result.task_id);
    pump_until(&mut app, &notice, Delivery::Submitted).await;
    assert!(kinds(&mut app).is_empty());
}

fn orbit_notice_events(app: &App) -> Vec<serde_json::Value> {
    app.event_hub
        .events_after(0)
        .into_iter()
        .filter(|(_, event)| event.event == crate::api::schema::EventKind::OrbitNotice)
        .map(|(_, event)| serde_json::to_value(event).unwrap())
        .collect()
}

#[tokio::test]
async fn notices_are_published_as_orbit_notice_api_events_without_the_summary() {
    let (mut app, _worker, _lead) = setup();
    // The API event is a runtime fact: it is published even with client toasts off.
    deliver_toasts(&mut app, ToastDelivery::Off);
    let result = assign(&mut app).await;
    assert!(report(&mut app, result.clone()).get("error").is_none());
    pump_until(&mut app, &result.task_id, Delivery::Submitted).await;
    pump(&mut app);
    assert!(kinds(&mut app).is_empty(), "toasts are off");
    let events = orbit_notice_events(&app);
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event["event"], "orbit_notice");
    let data = &event["data"];
    assert_eq!(data["type"], "orbit_notice");
    assert_eq!(data["kind"], "report");
    assert_eq!(data["worker"], "reviewer");
    assert_eq!(data["task_id"], result.task_id.as_str());
    assert!(data["pane_id"].is_string(), "{data}");
    assert!(data["message"].as_str().unwrap().contains(&result.task_id));
    assert!(!event.to_string().contains("Review complete"));
    assert_eq!(
        crate::api::schema::EventKind::OrbitNotice.dot_name(),
        "orbit.notice"
    );
}
