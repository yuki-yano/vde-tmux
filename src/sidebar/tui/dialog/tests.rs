use super::*;
use crate::sidebar::render::{SidebarRenderTheme, display_width};
use crate::sidebar::tui::draw::{category_dialog_lines, tail_display};
use crate::sidebar::tui::effects::CategoryIntentResult;

#[test]
fn category_text_dialog_waits_for_save_result_and_preserves_input_on_failure() {
    let (tx, rx) = mpsc::channel();
    let mut dialog = Some(CategoryDialog::editing(CategoryEditMode::Add {
        input: String::new(),
    }));
    let mut next_request_id = 7;
    let mut ui = MarkCompleteUi::default();
    for key in ['n', 'e', 'w'] {
        handle_category_edit_key(
            crossterm::event::KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
            &mut dialog,
            &tx,
            &mut next_request_id,
            &mut ui,
        );
    }
    handle_category_edit_key(
        crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut dialog,
        &tx,
        &mut next_request_id,
        &mut ui,
    );

    assert_eq!(
        dialog.as_ref().unwrap().phase,
        CategoryDialogPhase::Saving { request_id: 7 }
    );
    let request = rx.recv().unwrap();
    assert_eq!(request.dialog_request_id, Some(7));
    assert_eq!(
        request.intent,
        crate::category::CategoryIntent::CreateCategory {
            name: crate::category::CategoryName::parse("new").unwrap(),
        }
    );

    apply_category_intent_result(
        CategoryIntentResult {
            dialog_request_id: Some(7),
            result: Err(anyhow::anyhow!("state changed")),
        },
        &mut dialog,
        &mut ui,
    );
    let dialog_ref = dialog.as_ref().unwrap();
    assert_eq!(dialog_ref.phase, CategoryDialogPhase::Editing);
    assert_eq!(dialog_ref.error.as_deref(), Some("state changed"));
    assert!(matches!(
        &dialog_ref.edit,
        CategoryEditMode::Add { input } if input == "new"
    ));

    handle_category_edit_key(
        crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut dialog,
        &tx,
        &mut next_request_id,
        &mut ui,
    );
    assert_eq!(rx.recv().unwrap().dialog_request_id, Some(8));
    apply_category_intent_result(
        CategoryIntentResult {
            dialog_request_id: Some(8),
            result: Ok(()),
        },
        &mut dialog,
        &mut ui,
    );
    assert!(dialog.is_none());
    assert_eq!(ui.notice().unwrap().message, "category created");
}

#[test]
fn category_dialog_renders_delete_impact_choices_and_explicit_action() {
    let dialog = CategoryDialog::editing(CategoryEditMode::Delete {
        category: crate::category::CategoryName::parse("scratch").unwrap(),
        repository_count: 2,
        choices: vec![
            MembershipChoice::Automatic,
            MembershipChoice::Category(crate::category::CategoryName::parse("work").unwrap()),
        ],
        selected: 1,
        pending_g: false,
    });

    let lines = category_dialog_lines(&dialog, 38, 10, &SidebarRenderTheme::default());
    let rendered = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("Delete “scratch”?"));
    assert!(rendered.contains("2 repositories will be reassigned."));
    assert!(rendered.contains("Automatic (config)"));
    assert!(rendered.contains("› work"));
    assert!(rendered.contains("Enter Delete & move · Esc Cancel"));
    assert!(lines.iter().all(|line| {
        line.spans
            .iter()
            .map(|span| display_width(span.content.as_ref()))
            .sum::<usize>()
            <= 38
    }));

    let compact = category_dialog_lines(&dialog, 24, 3, &SidebarRenderTheme::default())
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(compact.contains("› work"));
    assert!(compact.contains("Enter Delete & move"));
}

#[test]
fn category_dialog_keeps_the_input_cursor_visible_at_narrow_widths() {
    assert_eq!(tail_display("› long-category-name_", 10), "…ory-name_");
}
