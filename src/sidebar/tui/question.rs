use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use crate::daemon::protocol::v2::ResolvedSnapshot;
use crate::pane_state::PaneInstance;
use crate::sidebar::tree::{SidebarRowKind, pane_instance_from_row_id};

use super::types::{MarkCompleteUi, NoticeLevel, SidebarView};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AckRequest {
    pub pane: PaneInstance,
    pub owner_ref: String,
    pub through_order: u64,
}

/// Freeze both target and fence from the rendered view, never the next incoming snapshot.
pub(super) fn displayed_target(
    snapshot: &ResolvedSnapshot,
    view: &SidebarView,
) -> Option<AckRequest> {
    let selection = view.state.selection.as_deref()?;
    let row = view.rows.iter().find(|row| row.id == selection)?;
    if !matches!(row.kind, SidebarRowKind::Chat | SidebarRowKind::Detail) {
        return None;
    }
    let pane = pane_instance_from_row_id(&row.id)?;
    let notice = snapshot
        .panes
        .iter()
        .find(|entry| entry.pane_instance == pane)?
        .question_notice
        .as_ref()?;
    if !notice.unacknowledged {
        return None;
    }
    Some(AckRequest {
        pane,
        owner_ref: notice.owner_ref.clone()?,
        through_order: notice.latest_order,
    })
}

pub(super) fn spawn_worker(
    socket: PathBuf,
    server: String,
) -> (mpsc::Sender<AckRequest>, mpsc::Receiver<anyhow::Result<()>>) {
    let (tx, rx) = mpsc::channel::<AckRequest>();
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let result = crate::sidebar::client::send_question_notice_ack_v2(
                &socket,
                &server,
                request.pane,
                request.owner_ref,
                request.through_order,
            );
            if result_tx.send(result).is_err() {
                return;
            }
        }
    });
    (tx, result_rx)
}

pub(super) fn queue(
    target: Option<&AckRequest>,
    connected: bool,
    pending: &mut bool,
    tx: &mpsc::Sender<AckRequest>,
    ui: &mut MarkCompleteUi,
) {
    let (message, level) = if !connected {
        (
            "接続待ちです。質問通知は確認済みになっていません。",
            NoticeLevel::Warning,
        )
    } else if *pending {
        ("質問通知を確認中です。", NoticeLevel::Progress)
    } else if let Some(target) = target {
        if tx.send(target.clone()).is_ok() {
            *pending = true;
            ("質問通知を確認中です。", NoticeLevel::Progress)
        } else {
            ("質問通知の確認処理を利用できません。", NoticeLevel::Failure)
        }
    } else {
        (
            "未確認の質問通知があるPaneを選択してください。",
            NoticeLevel::Warning,
        )
    };
    ui.set_toast(message.to_string(), level, Duration::from_secs(4));
}

pub(super) fn drain(
    rx: &mpsc::Receiver<anyhow::Result<()>>,
    pending: &mut bool,
    ui: &mut MarkCompleteUi,
) -> bool {
    let mut changed = false;
    while let Ok(result) = rx.try_recv() {
        *pending = false;
        changed = true;
        let (message, level) = match result {
            Ok(()) => (
                "表示していた質問通知を確認済みにしました。".to_string(),
                NoticeLevel::Success,
            ),
            Err(error) => (
                format!("質問通知を確認できませんでした: {error}"),
                NoticeLevel::Failure,
            ),
        };
        ui.set_toast(message, level, Duration::from_secs(5));
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::super::test_support::resolved_pane;
    use super::*;
    use crate::sidebar::state::{CategoryScope, PresentationMode, SidebarState};

    #[test]
    fn maximum_owner_summaries_fit_protocol_frame_and_keep_sidebar_counts_exact() {
        use crate::daemon::protocol::v2::{ServerMessage, encode_response_frame};
        let mut snapshot = ResolvedSnapshot {
            snapshot_revision: 1,
            panes: vec![],
            sidebar_model: Default::default(),
            attention: vec![],
            events: vec![],
            diagnostics: vec![],
        };
        for index in 0..crate::question_notice::MAX_OWNERS {
            let mut pane = resolved_pane(&format!("%{index}"), index as u32 + 1000, "$1");
            pane.question_notice = Some(crate::question_notice::QuestionNoticeSummary {
                unacknowledged: true,
                owner_ref: Some(format!("vtqn1:{index:064x}")),
                latest_order: 4096,
                last_issued_at: Some(i64::MAX),
                ..Default::default()
            });
            snapshot
                .sidebar_model
                .needs_action
                .insert(pane.pane_instance.clone());
            snapshot.panes.push(pane);
        }
        let state = SidebarState {
            category_scope: CategoryScope::All,
            presentation_mode: PresentationMode::Priority,
            ..Default::default()
        };
        let view = super::super::projection::project_view(&snapshot, &Default::default(), &state);
        let questions = view
            .rows
            .iter()
            .find(|row| row.label == "QUESTIONS")
            .unwrap();
        assert_eq!(questions.chat_count, 512);
        assert_eq!(questions.meta.as_ref().unwrap().question_count, 512);
        assert_eq!(
            view.rows
                .iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .count(),
            512
        );
        let frame = encode_response_frame(&ServerMessage::ResolvedSnapshotResult {
            snapshot_revision: 1,
            snapshot,
        })
        .unwrap();
        assert!(
            frame.len() < 2 * 1024 * 1024,
            "summaries must not include dedup history"
        );
    }

    #[test]
    fn acknowledgement_pins_displayed_target_and_order_and_disconnect_never_sends() {
        let mut pane = resolved_pane("%7", 700, "$1");
        pane.question_notice = Some(crate::question_notice::QuestionNoticeSummary {
            unacknowledged: true,
            owner_ref: Some("owner-one".into()),
            latest_order: 3,
            ..Default::default()
        });
        let mut snapshot = ResolvedSnapshot {
            snapshot_revision: 1,
            panes: vec![pane.clone()],
            sidebar_model: Default::default(),
            attention: vec![],
            events: vec![],
            diagnostics: vec![],
        };
        let state = SidebarState {
            category_scope: CategoryScope::All,
            presentation_mode: PresentationMode::Priority,
            selection: Some(crate::sidebar::tree::chat_row_id(&pane.pane_instance)),
            ..Default::default()
        };
        let view = super::super::projection::project_view(&snapshot, &Default::default(), &state);
        let target = displayed_target(&snapshot, &view).unwrap();
        snapshot.panes[0]
            .question_notice
            .as_mut()
            .unwrap()
            .latest_order = 4;
        let (tx, rx) = mpsc::channel();
        let mut pending = false;
        let mut ui = MarkCompleteUi::default();
        queue(Some(&target), false, &mut pending, &tx, &mut ui);
        assert!(rx.try_recv().is_err());
        assert!(!pending);
        queue(Some(&target), true, &mut pending, &tx, &mut ui);
        let request = rx.try_recv().unwrap();
        assert_eq!(request.pane, pane.pane_instance);
        assert_eq!(request.through_order, 3);
        assert!(pending);
        let mut parent_view = view.clone();
        parent_view.state.selection = Some("zone::priority::questions".into());
        assert!(displayed_target(&snapshot, &parent_view).is_none());
    }
}
