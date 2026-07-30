use agent_core::AgentEvent;
use agent_core::message::Message;

use super::ChatWidget;

#[test]
fn chatwidget_owns_live_and_restored_transcript_state() {
    let mut chat = ChatWidget::new(&[]);
    let mut state = crate::state::AppState::new("gpt-5", false);

    state.push_user("hello");
    chat.push_user_message(&state, "hello");
    state.apply(&AgentEvent::Text("bon".into()));
    chat.handle_agent_event(&state, &AgentEvent::Text("bon".into()));

    assert_eq!(chat.surface().transcript_cells().len(), 1);
    assert!(chat.surface().active_cell().is_some());

    state.apply(&AgentEvent::EndTurn);
    chat.handle_agent_event(&state, &AgentEvent::EndTurn);

    assert_eq!(chat.surface().transcript_cells().len(), 2);
    assert!(chat.surface().active_cell().is_none());

    chat.replace_messages(&[Message::user("restored")]);

    assert_eq!(chat.surface().transcript_cells().len(), 1);
    assert!(chat.surface().active_cell().is_none());
}

#[test]
fn chatwidget_mirrors_local_feedback_without_replaying_engine_blocks() {
    let mut chat = ChatWidget::new(&[]);
    let mut state = crate::state::AppState::new("gpt-5", false);

    state.push_user("hello");
    chat.push_user_message(&state, "hello");
    state
        .blocks
        .push(crate::state::Block::Notice("copied".into()));
    state
        .blocks
        .push(crate::state::Block::Error("failed".into()));

    chat.sync_local_blocks(&state);

    assert_eq!(chat.surface().transcript_cells().len(), 3);
}
