use super::*;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn list_selection_skips_disabled_rows_and_accepts_enabled() {
    let rows = vec![
        SelectionRow::new("a", "Alpha", ""),
        SelectionRow::new("b", "Beta", "").disabled(),
        SelectionRow::new("c", "Gamma", ""),
    ];
    let mut view = ListSelectionView::new("Pick", rows);

    view.handle_key_event(key(KeyCode::Down));
    assert_eq!(view.selected_row().map(|row| row.id.as_str()), Some("c"));
    view.handle_key_event(key(KeyCode::Enter));
    assert_eq!(view.completion(), ViewCompletion::Accepted);
}

#[test]
fn list_selection_search_tabs_paste_and_escape_are_routed() {
    let mut view = ListSelectionView::new(
        "Pick",
        vec![
            SelectionRow::new("one", "One", "stable"),
            SelectionRow::new("two", "Two", "target"),
        ],
    )
    .with_tabs(vec![
        SelectionTab::new("all", "All"),
        SelectionTab::new("recent", "Recent"),
    ]);

    assert!(view.handle_key_event(key(KeyCode::Tab)));
    assert_eq!(view.active_tab_id(), Some("recent"));
    assert!(view.handle_paste("tar"));
    assert_eq!(view.query(), "tar");
    assert_eq!(view.selected_row().map(|row| row.id.as_str()), Some("two"));
    assert!(view.handle_key_event(key(KeyCode::Esc)));
    assert_eq!(view.completion(), ViewCompletion::Dismissed);
}

#[test]
fn list_selection_does_not_accept_invisible_filtered_row() {
    let mut view = ListSelectionView::new(
        "Pick",
        vec![
            SelectionRow::new("one", "One", ""),
            SelectionRow::new("two", "Two", ""),
        ],
    );

    assert!(view.handle_paste("missing"));
    assert!(view.selected_row().is_none());
    assert!(view.handle_key_event(key(KeyCode::Enter)));
    assert_eq!(view.completion(), ViewCompletion::Pending);
}

#[test]
fn bottom_pane_routes_views_before_composer() {
    let mut state = AppState::new("gpt-5", false);
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(ListSelectionView::new(
        "Pick",
        vec![SelectionRow::new("x", "Exit", "")],
    )));

    assert_eq!(
        pane.route_key(&mut state, key(KeyCode::Enter)),
        InputAction::None
    );
    assert_eq!(pane.depth(), 0);
    assert!(state.input.is_empty());

    assert!(pane.route_paste(&mut state, "hello"));
    assert_eq!(state.input, "hello");
}

#[test]
fn bottom_pane_renders_active_view() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(ListSelectionView::new(
        "Pick",
        vec![SelectionRow::new("x", "Exit", "")],
    )));
    let mut buf = Buffer::empty(Rect::new(0, 0, 30, 4));

    assert!(pane.render(Rect::new(0, 0, 30, 4), &mut buf));
    let text = (0..4)
        .map(|y| (0..30).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Pick"));
}

#[test]
fn bottom_pane_parent_can_dismiss_after_child_accept() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(
        ListSelectionView::new("Parent", vec![SelectionRow::new("p", "Parent", "")])
            .dismiss_parent_on_accept(),
    ));
    pane.push_view(Box::new(ListSelectionView::new(
        "Child",
        vec![SelectionRow::new("c", "Child", "")],
    )));

    let mut state = AppState::new("gpt-5", false);
    pane.route_key(&mut state, key(KeyCode::Enter));

    assert_eq!(pane.depth(), 0);
}
