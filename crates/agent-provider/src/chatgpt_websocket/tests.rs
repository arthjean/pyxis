use super::*;

#[test]
fn a_new_session_cancels_active_work_and_mints_a_new_generation() {
    let websocket = ResponsesWebSocket::new();
    let first = websocket.scope_snapshot();

    websocket.reset_scope();

    let second = websocket.scope_snapshot();
    assert!(first.cancelled.is_cancelled());
    assert_ne!(first.generation, second.generation);
    assert!(!second.cancelled.is_cancelled());
}
