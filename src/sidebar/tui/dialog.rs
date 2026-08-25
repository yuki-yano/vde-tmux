use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::daemon::protocol::v2::ResolvedSnapshot;

use super::effects::{
    CategoryIntentRequest, CategoryIntentResult, category_name_from_row_id,
    category_repo_from_row_id,
};
use super::types::{MarkCompleteUi, NoticeLevel, SidebarView};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CategoryEditMode {
    Add {
        input: String,
    },
    Rename {
        current: crate::category::CategoryName,
        input: String,
    },
    MoveRepo {
        repo: crate::category::RepoKey,
        repo_label: String,
        choices: Vec<MembershipChoice>,
        selected: usize,
        pending_g: bool,
    },
    Delete {
        category: crate::category::CategoryName,
        repository_count: usize,
        choices: Vec<MembershipChoice>,
        selected: usize,
        pending_g: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MembershipChoice {
    Automatic,
    Category(crate::category::CategoryName),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CategoryDialogPhase {
    Editing,
    Saving { request_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CategoryDialog {
    pub(super) edit: CategoryEditMode,
    pub(super) phase: CategoryDialogPhase,
    pub(super) error: Option<String>,
}

impl CategoryDialog {
    pub(super) fn editing(edit: CategoryEditMode) -> Self {
        Self {
            edit,
            phase: CategoryDialogPhase::Editing,
            error: None,
        }
    }

    pub(super) fn title(&self) -> &'static str {
        match &self.edit {
            CategoryEditMode::Add { .. } => " ADD CATEGORY ",
            CategoryEditMode::Rename { .. } => " RENAME CATEGORY ",
            CategoryEditMode::MoveRepo { .. } => " MOVE REPOSITORY ",
            CategoryEditMode::Delete { .. } => " DELETE CATEGORY ",
        }
    }

    pub(super) fn action_hint(&self) -> &'static str {
        match self.phase {
            CategoryDialogPhase::Saving { .. } => "Saving…",
            CategoryDialogPhase::Editing => match &self.edit {
                CategoryEditMode::Add { .. } => "Enter Create · Esc Cancel",
                CategoryEditMode::Rename { .. } => "Enter Rename · Esc Cancel",
                CategoryEditMode::MoveRepo { .. } => "Enter Move · Esc Cancel",
                CategoryEditMode::Delete { .. } => "Enter Delete & move · Esc Cancel",
            },
        }
    }

    pub(super) fn success_message(&self) -> &'static str {
        match &self.edit {
            CategoryEditMode::Add { .. } => "category created",
            CategoryEditMode::Rename { .. } => "category renamed",
            CategoryEditMode::MoveRepo { .. } => "repository moved",
            CategoryEditMode::Delete { .. } => "category deleted",
        }
    }

    pub(super) fn saving_request_id(&self) -> Option<u64> {
        match self.phase {
            CategoryDialogPhase::Editing => None,
            CategoryDialogPhase::Saving { request_id } => Some(request_id),
        }
    }
}

impl MembershipChoice {
    pub(super) fn label(&self) -> &str {
        match self {
            Self::Automatic => crate::category::AUTOMATIC_LABEL,
            Self::Category(category) => category.as_str(),
        }
    }

    pub(super) fn target(&self) -> crate::category::MembershipTarget {
        match self {
            Self::Automatic => crate::category::MembershipTarget::Automatic,
            Self::Category(category) => {
                crate::category::MembershipTarget::Category(category.clone())
            }
        }
    }
}

pub(super) fn apply_category_intent_result(
    result: CategoryIntentResult,
    dialog: &mut Option<CategoryDialog>,
    ui: &mut MarkCompleteUi,
) {
    let is_dialog_result = result.dialog_request_id.is_some_and(|request_id| {
        dialog.as_ref().and_then(CategoryDialog::saving_request_id) == Some(request_id)
    });
    if !is_dialog_result {
        if let Err(error) = result.result {
            ui.set_toast(
                format!("category save failed: {error}"),
                NoticeLevel::Failure,
                Duration::from_secs(5),
            );
        }
        return;
    }

    match result.result {
        Ok(()) => {
            let message = dialog
                .as_ref()
                .map(CategoryDialog::success_message)
                .unwrap_or("category saved")
                .to_string();
            *dialog = None;
            ui.set_toast(message, NoticeLevel::Success, Duration::from_secs(3));
        }
        Err(error) => {
            if let Some(dialog) = dialog.as_mut() {
                dialog.phase = CategoryDialogPhase::Editing;
                dialog.error = Some(error.to_string());
            }
        }
    }
}

pub(super) fn begin_category_edit(
    key: char,
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
    dialog: &mut Option<CategoryDialog>,
    ui: &mut MarkCompleteUi,
) -> bool {
    let selected = sidebar
        .state
        .selection
        .as_deref()
        .and_then(|selection| sidebar.rows.iter().find(|row| row.id == selection));
    let edit = match key {
        'a' => CategoryEditMode::Add {
            input: String::new(),
        },
        'r' => {
            let Some(category) = selected.and_then(|row| category_name_from_row_id(&row.id)) else {
                ui.set_toast(
                    "select a category to rename".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(4),
                );
                return true;
            };
            let editable = snapshot
                .sidebar_model
                .categories
                .category(&category)
                .is_some_and(|category| {
                    category.source == crate::category::CategorySource::Dynamic
                });
            if !editable {
                ui.set_toast(
                    "only dynamic categories can be renamed".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(4),
                );
                return true;
            }
            CategoryEditMode::Rename {
                current: category,
                input: String::new(),
            }
        }
        'm' => {
            let Some(row) = selected else {
                ui.set_toast(
                    "select a repository to move".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(4),
                );
                return true;
            };
            let Some((_, repo)) = category_repo_from_row_id(&row.id) else {
                ui.set_toast(
                    "select a repository to move".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(4),
                );
                return true;
            };
            CategoryEditMode::MoveRepo {
                repo,
                repo_label: row.label.clone(),
                choices: membership_choices(snapshot, None),
                selected: 0,
                pending_g: false,
            }
        }
        'D' => {
            let Some(category) = selected.and_then(|row| category_name_from_row_id(&row.id)) else {
                ui.set_toast(
                    "select a category to delete".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(4),
                );
                return true;
            };
            let editable = snapshot
                .sidebar_model
                .categories
                .category(&category)
                .is_some_and(|candidate| {
                    candidate.source == crate::category::CategorySource::Dynamic
                });
            if !editable {
                ui.set_toast(
                    "only dynamic categories can be deleted".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(4),
                );
                return true;
            }
            let repository_count = snapshot
                .sidebar_model
                .categories
                .placements
                .values()
                .filter(|placement| placement.category == category)
                .count();
            CategoryEditMode::Delete {
                choices: membership_choices(snapshot, Some(&category)),
                category,
                repository_count,
                selected: 0,
                pending_g: false,
            }
        }
        _ => return false,
    };
    *dialog = Some(CategoryDialog::editing(edit));
    true
}

fn membership_choices(
    snapshot: &ResolvedSnapshot,
    exclude: Option<&crate::category::CategoryName>,
) -> Vec<MembershipChoice> {
    std::iter::once(MembershipChoice::Automatic)
        .chain(
            snapshot
                .sidebar_model
                .categories
                .categories
                .iter()
                .filter(|category| exclude != Some(&category.name))
                .map(|category| MembershipChoice::Category(category.name.clone())),
        )
        .collect()
}

pub(super) fn handle_category_edit_key(
    key: crossterm::event::KeyEvent,
    dialog: &mut Option<CategoryDialog>,
    tx: &mpsc::Sender<CategoryIntentRequest>,
    next_request_id: &mut u64,
    ui: &mut MarkCompleteUi,
) -> bool {
    let Some(current) = dialog.as_mut() else {
        return false;
    };
    if matches!(current.phase, CategoryDialogPhase::Saving { .. }) {
        return true;
    }
    if key.code == KeyCode::Esc {
        *dialog = None;
        ui.set_toast(
            "category edit cancelled".to_string(),
            NoticeLevel::Warning,
            Duration::from_secs(2),
        );
        return true;
    }
    current.error = None;
    let mut intent = None;
    match &mut current.edit {
        CategoryEditMode::Add { input } => match key.code {
            KeyCode::Enter => match crate::category::CategoryName::parse(input.as_str()) {
                Ok(name) => intent = Some(crate::category::CategoryIntent::CreateCategory { name }),
                Err(error) => {
                    current.error = Some(error);
                    return true;
                }
            },
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.push(ch),
            _ => {}
        },
        CategoryEditMode::Rename {
            current: current_name,
            input,
        } => match key.code {
            KeyCode::Enter => match crate::category::CategoryName::parse(input.as_str()) {
                Ok(replacement) => {
                    intent = Some(crate::category::CategoryIntent::RenameCategory {
                        current: current_name.clone(),
                        replacement,
                    })
                }
                Err(error) => {
                    current.error = Some(error);
                    return true;
                }
            },
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.push(ch),
            _ => {}
        },
        CategoryEditMode::MoveRepo {
            repo,
            choices,
            selected,
            pending_g,
            ..
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.saturating_sub(1);
                *pending_g = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1).min(choices.len().saturating_sub(1));
                *pending_g = false;
            }
            KeyCode::Char('g') => {
                if *pending_g {
                    *selected = 0;
                    *pending_g = false;
                } else {
                    *pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                *selected = choices.len().saturating_sub(1);
                *pending_g = false;
            }
            KeyCode::Enter => {
                intent = choices.get(*selected).map(|choice| match choice {
                    MembershipChoice::Automatic => {
                        crate::category::CategoryIntent::SetRepoAutomatic { repo: repo.clone() }
                    }
                    MembershipChoice::Category(category) => {
                        crate::category::CategoryIntent::AssignRepo {
                            repo: repo.clone(),
                            category: category.clone(),
                        }
                    }
                });
            }
            _ => *pending_g = false,
        },
        CategoryEditMode::Delete {
            category,
            choices,
            selected,
            pending_g,
            ..
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.saturating_sub(1);
                *pending_g = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1).min(choices.len().saturating_sub(1));
                *pending_g = false;
            }
            KeyCode::Char('g') => {
                if *pending_g {
                    *selected = 0;
                    *pending_g = false;
                } else {
                    *pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                *selected = choices.len().saturating_sub(1);
                *pending_g = false;
            }
            KeyCode::Enter => {
                intent = choices.get(*selected).map(|choice| {
                    crate::category::CategoryIntent::DeleteCategory {
                        category: category.clone(),
                        replacement: choice.target(),
                    }
                });
            }
            _ => *pending_g = false,
        },
    }
    if let Some(intent) = intent {
        let request_id = *next_request_id;
        *next_request_id = (*next_request_id).saturating_add(1);
        if tx
            .send(CategoryIntentRequest {
                intent,
                dialog_request_id: Some(request_id),
            })
            .is_err()
        {
            current.error = Some("category worker unavailable".to_string());
        } else {
            current.phase = CategoryDialogPhase::Saving { request_id };
        }
    }
    true
}
