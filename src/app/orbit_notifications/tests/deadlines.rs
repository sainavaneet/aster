//! Bounded delivery waits with controlled time: an absent lead fails a queued
//! report after `lead_absent_timeout_ms`, an unverifiable uncertain delivery is
//! requeued once then failed, and a held decision is released on the tick.
use super::super::deadlines::{LEAD_ABSENT_FAIL_MS, UNCERTAIN_RESOLVE_MS};
use super::super::deliver::POSSIBLE_DUPLICATE;
use super::*;
use crate::config::{OrbitNotifyEvent, ToastDelivery};

const ABSENT: Duration = Duration::from_millis(LEAD_ABSENT_FAIL_MS);
const UNRESOLVED: Duration = Duration::from_millis(UNCERTAIN_RESOLVE_MS);

async fn reported(app: &mut App) -> (String, String) {
    let result = assign(app).await;
    assert!(report(app, result.clone()).get("error").is_none());
    let lead_id = task(app, &result.task_id).lead_id.clone();
    (result.task_id, lead_id)
}

/// The lead pane runs a shell: no known agent hosts its composer.
fn lead_at_shell(app: &mut App, lead_id: &str) {
    app.state
        .terminals
        .get_mut(lead_id)
        .unwrap()
        .set_detected_state(None, AgentState::Idle);
}

fn failed_notices(app: &mut App) -> usize {
    app.take_orbit_human_notices()
        .into_iter()
        .filter(|notice| notice.event == OrbitNotifyEvent::Failed)
        .count()
}

#[tokio::test]
async fn queued_report_fails_visibly_after_the_lead_stays_absent() {
    let (mut app, _worker, mut lead) = setup();
    app.state.toast_config.delivery = ToastDelivery::Aster;
    let (task_id, lead_id) = reported(&mut app).await;
    lead_at_shell(&mut app, &lead_id);
    let start = Instant::now();
    pump_at(&mut app, start);
    pump_at(&mut app, start + ABSENT - Duration::from_millis(1));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Queued);
    assert_eq!(
        task(&app, &task_id).attempts,
        0,
        "waiting is not an attempt"
    );
    pump_at(&mut app, start + ABSENT);
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Failed);
    let error = stored.error.as_deref().unwrap();
    assert!(
        error.contains("absent or not running its agent for 30 minutes"),
        "{error}"
    );
    assert_eq!(failed_notices(&mut app), 1);
    // A lead that comes back later gets nothing: the failure is final.
    set_state(&mut app, &lead_id, AgentState::Idle);
    pump_at(&mut app, start + ABSENT + Duration::from_secs(60));
    assert!(lead.try_recv().is_err());
}

#[tokio::test]
async fn a_returning_lead_restarts_the_absence_clock_and_config_sets_the_bound() {
    let (mut app, _worker, _lead) = setup();
    let mut config = crate::config::PolarisConfig::default();
    config.delivery.lead_absent_timeout_ms = 60_000;
    app.configure_orbit_notifications(&config);
    let (task_id, lead_id) = reported(&mut app).await;
    // Busy but present: no absence clock at all.
    set_state(&mut app, &lead_id, AgentState::Working);
    let start = Instant::now();
    pump_at(&mut app, start);
    pump_at(&mut app, start + Duration::from_secs(120));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Queued);
    // Absent 50 s, back, absent again 50 s: never 60 s in a row.
    lead_at_shell(&mut app, &lead_id);
    pump_at(&mut app, start + Duration::from_secs(130));
    pump_at(&mut app, start + Duration::from_secs(180));
    set_state(&mut app, &lead_id, AgentState::Working);
    pump_at(&mut app, start + Duration::from_secs(181));
    lead_at_shell(&mut app, &lead_id);
    pump_at(&mut app, start + Duration::from_secs(182));
    pump_at(&mut app, start + Duration::from_secs(232));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Queued);
    pump_at(&mut app, start + Duration::from_secs(242));
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Failed);
    assert!(stored.error.as_deref().unwrap().contains("1 minutes"));
}

#[tokio::test]
async fn uncertain_delivery_to_a_blocked_lead_is_requeued_once_then_fails() {
    let (mut app, _worker, mut lead) = setup();
    let (task_id, lead_id) = reported(&mut app).await;
    fail_delivery_ack(&mut app, &task_id, 1);
    set_state(&mut app, &lead_id, AgentState::Blocked);
    let start = Instant::now();
    pump_at(&mut app, start);
    assert_eq!(task(&app, &task_id).delivery, Delivery::Uncertain);
    pump_at(&mut app, start + UNRESOLVED - Duration::from_millis(1));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Uncertain);
    pump_at(&mut app, start + UNRESOLVED);
    let stored = task(&app, &task_id);
    assert_eq!(
        stored.delivery,
        Delivery::Queued,
        "requeued, lead still blocked"
    );
    assert!(stored.redelivered);
    assert_eq!(stored.recoveries, 1);
    // Once the lead is idle the possible duplicate goes out.
    set_state(&mut app, &lead_id, AgentState::Idle);
    let later = start + UNRESOLVED + Duration::from_secs(1);
    pump_at(&mut app, later);
    assert_eq!(task(&app, &task_id).delivery, Delivery::Submitting);
    for _ in 0..200 {
        let now = app.orbit_notifications.now_ms(later);
        app.orbit_notifications.reap_acks(now);
        if task(&app, &task_id).delivery == Delivery::Submitted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(drain(&mut lead).contains(POSSIBLE_DUPLICATE));
    // A second unverifiable delivery is never resent: it fails.
    fail_delivery_ack(&mut app, &task_id, 2);
    set_state(&mut app, &lead_id, AgentState::Unknown);
    let second = later + Duration::from_secs(1);
    pump_at(&mut app, second);
    assert_eq!(task(&app, &task_id).delivery, Delivery::Uncertain);
    pump_at(&mut app, second + UNRESOLVED);
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Failed);
    assert!(stored
        .error
        .as_deref()
        .unwrap()
        .contains("could not be verified"));
    assert!(lead.try_recv().is_err());
}
