use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::pane_state::PaneInstance;
use crate::sidebar::render::HeaderLayout;
use crate::sidebar::state::SidebarState;
use crate::sidebar::tree::{BadgeCounts, SidebarRow};

/// Geometry and row mapping of the most recently drawn frame. Click hit-testing
/// must use exactly what was drawn, so the run loop records it on every draw.
#[derive(Debug, Clone)]
pub(super) struct DrawnFrame {
    pub(super) header: HeaderLayout,
    pub(super) header_rows: u16,
    pub(super) rows_height: u16,
    pub(super) width: u16,
    pub(super) scroll: usize,
    pub(super) row_indices: Vec<Option<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct SidebarView {
    pub(super) state: SidebarState,
    pub(super) rows: Vec<SidebarRow>,
    pub(super) counts: BadgeCounts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NavigationRequest {
    pub(super) selection: Option<String>,
    pub(super) scroll: usize,
    pub(super) manual_scroll: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoticeLevel {
    Success,
    Progress,
    Warning,
    Failure,
}

#[derive(Debug, Clone)]
struct ToastNotice {
    message: String,
    level: NoticeLevel,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Notice<'a> {
    pub(super) message: &'a str,
    pub(super) level: NoticeLevel,
}

#[derive(Debug, Default)]
pub(super) struct MarkCompleteUi {
    pub(super) pending: BTreeSet<PaneInstance>,
    pub(super) pin_pending: BTreeSet<PaneInstance>,
    toast: Option<(ToastNotice, Instant)>,
}

impl MarkCompleteUi {
    pub(super) fn notice(&self) -> Option<Notice<'_>> {
        self.toast
            .as_ref()
            .filter(|(_, expires)| *expires > Instant::now())
            .map(|(toast, _)| Notice {
                message: toast.message.as_str(),
                level: toast.level,
            })
            .or_else(|| {
                (!self.pending.is_empty()).then_some(Notice {
                    message: "marking complete...",
                    level: NoticeLevel::Progress,
                })
            })
            .or_else(|| {
                (!self.pin_pending.is_empty()).then_some(Notice {
                    message: "updating pin...",
                    level: NoticeLevel::Progress,
                })
            })
    }

    pub(super) fn set_toast(&mut self, message: String, level: NoticeLevel, duration: Duration) {
        self.toast = Some((ToastNotice { message, level }, Instant::now() + duration));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClickAction {
    JumpPane(PaneInstance),
}

pub(super) struct ClickContext<'a> {
    pub(super) socket: &'a Path,
    pub(super) server_identity: &'a str,
    pub(super) source_pane: &'a PaneInstance,
}

pub(super) fn rendered_row_range(
    row_indices: &[Option<usize>],
    row_index: usize,
) -> Option<(usize, usize)> {
    let start = row_indices
        .iter()
        .position(|mapped| *mapped == Some(row_index))?;
    let end = row_indices
        .iter()
        .rposition(|mapped| *mapped == Some(row_index))?;
    Some((start, end))
}
