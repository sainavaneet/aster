//! Delivery gate: client input quiet window, visible idle prompt, idle fallback.
use super::super::gate::{DRAFT_QUIET_MS, IDLE_FALLBACK_MS, USER_INPUT_QUIET_MS};
use super::*;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

async fn queued(app: &mut App) -> String {
    let result = assign(app).await;
    assert!(report(app, result.clone()).get("error").is_none());
    result.task_id
}

#[tokio::test]
async fn visible_idle_lead_receives_report_on_the_first_tick() {
    let (mut app, _worker, _lead) = setup();
    let task_id = queued(&mut app).await;
    assert!(app.state.terminals[lead_id(&app).as_str()].visible_idle());
    pump(&mut app);
    assert_eq!(task(&app, &task_id).delivery, Delivery::Submitting);
}

#[tokio::test]
async fn recent_client_typing_on_the_lead_defers_delivery() {
    let (mut app, _worker, mut lead) = setup();
    let task_id = queued(&mut app).await;
    let lead_id = lead_id(&app);
    let typed = Instant::now();
    type_into(&mut app, &lead_id, typed);
    pump_at(&mut app, typed + ms(USER_INPUT_QUIET_MS / 2));
    pump_at(&mut app, typed + ms(USER_INPUT_QUIET_MS));
    let stored = task(&app, &task_id);
    assert_eq!(stored.delivery, Delivery::Queued);
    assert_eq!(stored.attempts, 0, "a deferral is not an attempt");
    assert!(lead.try_recv().is_err());
    pump_at(&mut app, typed + ms(USER_INPUT_QUIET_MS + 1));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Submitting);
}

#[tokio::test]
async fn lead_without_visible_idle_receives_report_only_after_the_fallback() {
    let (mut app, _worker, mut lead) = setup();
    let task_id = queued(&mut app).await;
    let lead_id = lead_id(&app);
    set_screen(&mut app, &lead_id, AgentState::Idle, false);
    let start = Instant::now();
    pump_at(&mut app, start);
    pump_at(&mut app, start + ms(IDLE_FALLBACK_MS - 1));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Queued);
    assert!(lead.try_recv().is_err());
    pump_at(&mut app, start + ms(IDLE_FALLBACK_MS));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Submitting);
}

#[tokio::test]
async fn leaving_idle_restarts_the_fallback_period() {
    let (mut app, _worker, _lead) = setup();
    let task_id = queued(&mut app).await;
    let lead_id = lead_id(&app);
    set_screen(&mut app, &lead_id, AgentState::Idle, false);
    let start = Instant::now();
    pump_at(&mut app, start);
    set_screen(&mut app, &lead_id, AgentState::Working, false);
    pump_at(&mut app, start + ms(IDLE_FALLBACK_MS / 2));
    set_screen(&mut app, &lead_id, AgentState::Idle, false);
    let idle_again = start + ms(IDLE_FALLBACK_MS / 2 + 1_000);
    pump_at(&mut app, idle_again);
    // Ten seconds after the first idle, but not after the latest one.
    pump_at(&mut app, start + ms(IDLE_FALLBACK_MS + 1_000));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Queued);
    pump_at(&mut app, idle_again + ms(IDLE_FALLBACK_MS));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Submitting);
}

#[tokio::test]
async fn unsent_draft_defers_delivery_past_the_normal_quiet_window() {
    let (mut app, _worker, mut lead) = setup();
    let task_id = queued(&mut app).await;
    let lead_id = lead_id(&app);
    let typed = Instant::now();
    type_bytes(&mut app, &lead_id, b"refactor the parser", typed);
    pump_at(&mut app, typed + ms(3_000));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Queued);
    pump_at(&mut app, typed + ms(DRAFT_QUIET_MS));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Queued);
    assert_eq!(task(&app, &task_id).attempts, 0);
    assert!(lead.try_recv().is_err());
    // Liveness: a draft left alone for over a minute no longer blocks delivery.
    pump_at(&mut app, typed + ms(DRAFT_QUIET_MS + 1_000));
    assert_eq!(task(&app, &task_id).delivery, Delivery::Submitting);
}

#[tokio::test]
async fn submitted_or_cancelled_draft_uses_the_normal_quiet_window() {
    for ending in [b"\r".as_slice(), b"\x03", b"\x15", b"\x1b"] {
        let (mut app, _worker, _lead) = setup();
        let task_id = queued(&mut app).await;
        let lead_id = lead_id(&app);
        let typed = Instant::now();
        type_bytes(&mut app, &lead_id, b"draft", typed);
        type_bytes(&mut app, &lead_id, ending, typed);
        assert!(!app.state.terminals[lead_id.as_str()].draft_pending());
        pump_at(&mut app, typed + ms(USER_INPUT_QUIET_MS));
        assert_eq!(
            task(&app, &task_id).delivery,
            Delivery::Queued,
            "{ending:?}"
        );
        pump_at(&mut app, typed + ms(USER_INPUT_QUIET_MS + 1));
        assert_eq!(
            task(&app, &task_id).delivery,
            Delivery::Submitting,
            "{ending:?}"
        );
    }
}
