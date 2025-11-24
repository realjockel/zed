mod deadline_notification;

use chrono::{Local, NaiveTime, Timelike};
use deadline_notification::{DeadlineNotification, DeadlineNotificationEvent};
use editor::scroll::Autoscroll;
use editor::{self, Editor, SelectionEffects, ToPoint};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Global, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    WeakEntity, Window, actions,
};
use language::ToOffset as _;
use language::{Point, ToPoint as _};
use lsp::CompletionContext;
use menu;
use multi_buffer::{ExcerptRange, MultiBufferSnapshot, ToOffset as _};
use picker::{Picker, PickerDelegate};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};
pub use settings::HourFormat;
use settings::{ActiveSettingsProfileName, CaptureTemplateConfig, RegisterSetting, Settings};
use std::{
    fs::OpenOptions,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use theme::ActiveTheme;
use tree_sitter;
use ui::{HighlightedLabel, Label, LabelSize, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use util::paths::PathStyle;
use util::rel_path::RelPathBuf;
use workspace::{AppState, ModalView, OpenVisible, Workspace, item::ItemEvent};

actions!(
    journal,
    [
        /// Creates a new journal entry for today.
        NewJournalEntry,
        /// Opens the journal entry picker.
        OpenJournalEntry,
        /// Opens the capture template picker.
        CaptureTemplate,
        /// Toggles zen mode for journal writing (hides all panels and centers text).
        ToggleJournalZenMode,
        /// Focuses on the current section (folds all other sections at the same level).
        FocusSection,
        /// Unfocuses and shows all sections.
        UnfocusSection,
        /// Cycles fold state of current section (folded -> children -> expanded).
        CycleFold,
        /// Cycles the TODO state of the current headline (TODO -> DOING -> DONE, etc.).
        CycleTodoState,
        /// Schedules the current task with a date.
        ScheduleTask,
        /// Sets a deadline for the current task.
        SetDeadline,
        /// Adds or modifies recurring interval for a scheduled/deadline task.
        SetRecurring,
        /// Decreases the date by one week in the date picker.
        DatePickerPreviousWeek,
        /// Increases the date by one week in the date picker.
        DatePickerNextWeek,
        /// Cycles through reminder options in the date picker (None -> 10m -> 1h -> 1d -> 1w).
        DatePickerCycleReminder,
        /// Cycles through repeater/recurring options in the date picker (None -> +1d -> +1w -> +2w -> +1m -> +3m -> +1y -> ++1d -> ++1w -> .+1d -> .+1w).
        DatePickerCycleRepeater,
        /// Opens the agenda view showing scheduled and deadline tasks.
        OpenAgenda,
        /// Toggles showing completed (DONE/CLOSED) items in agenda.
        AgendaToggleDone,
        /// Cycles through date range filters (Week/Month/Quarter/All).
        AgendaCycleDateRange,
        /// Cycles through tag filters (All/specific tags).
        AgendaCycleTagFilter,
        /// Cycles through file filters (All/specific files).
        AgendaCycleFileFilter,
        /// Clears all agenda filters.
        AgendaClearFilters,
        /// Opens a picker to select a tag filter with fuzzy search.
        AgendaPickTagFilter,
        /// Opens a picker to select a file filter with fuzzy search.
        AgendaPickFileFilter,
        /// Opens a picker to select a date range filter.
        AgendaPickDateRange,
    ]
);

/// Settings specific to journaling
#[derive(Clone, Debug, RegisterSetting)]
pub struct JournalSettings {
    /// The path of the directory where journal entries are stored.
    ///
    /// Default: `~`
    pub path: String,
    /// What format to display the hours in.
    ///
    /// Default: hour12
    pub hour_format: HourFormat,
    /// Capture templates for quick note-taking
    pub capture_templates: Vec<CaptureTemplateConfig>,
    /// TODO keywords that represent active states
    pub todo_keywords: Vec<String>,
    /// TODO keywords that represent done/completed states
    pub done_keywords: Vec<String>,
    /// Whether to add a timestamp when a task is marked as done
    pub timestamp_on_done: bool,
}

impl settings::Settings for JournalSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let journal = content.journal.clone().unwrap();

        Self {
            path: journal.path.unwrap(),
            hour_format: journal.hour_format.unwrap(),
            capture_templates: journal.capture_templates.unwrap_or_default(),
            todo_keywords: journal.todo_keywords.unwrap_or_else(|| {
                vec![
                    "TODO".to_string(),
                    "DOING".to_string(),
                    "WAITING".to_string(),
                ]
            }),
            done_keywords: journal
                .done_keywords
                .unwrap_or_else(|| vec!["DONE".to_string(), "CANCELLED".to_string()]),
            timestamp_on_done: journal.timestamp_on_done.unwrap_or(true),
        }
    }
}

/// Stores the state of UI elements before zen mode is enabled
#[derive(Clone, Debug, Default)]
struct JournalZenModeState {
    is_active: bool,
    journal_dir: Option<PathBuf>,
    previous_profile: Option<String>,
}

impl Global for JournalZenModeState {}

const JOURNAL_ZEN_PROFILE: &str = "journal-zen";

pub fn init(_: Arc<AppState>, cx: &mut App) {
    cx.set_global(JournalZenModeState::default());

    // Register tag completion provider for org-mode files
    cx.observe_new(|editor: &mut Editor, _window, cx| {
        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
            return;
        };

        let Some(file) = buffer.read(cx).file() else {
            return;
        };

        let path = file.path();
        log::info!("Journal: Checking file for tag completion: {:?}", path);

        if let Some(extension) = path.extension() {
            log::info!("Journal: File extension: {:?}", extension);
            if extension == "org" {
                log::info!("Journal: Setting tag completion provider for {:?}", path);
                // This is an org-mode file, register tag completion
                editor
                    .set_completion_provider(Some(std::rc::Rc::new(TagCompletionProvider::new())));
            }
        }
    })
    .detach();

    cx.observe_new(
        |workspace: &mut Workspace, window, cx: &mut Context<Workspace>| {
            let Some(window) = window else {
                return;
            };
            workspace.register_action(|workspace, _: &NewJournalEntry, window, cx| {
                new_journal_entry(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &OpenJournalEntry, window, cx| {
                open_journal_entry(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &CaptureTemplate, window, cx| {
                capture_template(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &ToggleJournalZenMode, window, cx| {
                toggle_journal_zen_mode(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &FocusSection, window, cx| {
                focus_section(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &UnfocusSection, window, cx| {
                unfocus_section(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &CycleFold, window, cx| {
                cycle_fold(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &CycleTodoState, window, cx| {
                cycle_todo_state(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &ScheduleTask, window, cx| {
                schedule_task(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &SetDeadline, window, cx| {
                set_deadline(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &SetRecurring, window, cx| {
                set_recurring(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &OpenAgenda, window, cx| {
                open_agenda(workspace, window, cx);
            });

            // Observe active pane changes to automatically enable/disable zen mode
            let active_pane = workspace.active_pane().clone();
            cx.observe_in(&active_pane, window, |workspace, _pane, window, cx| {
                handle_active_item_change(workspace, window, cx);
            })
            .detach();

            // Also call it immediately for any already-open files
            handle_active_item_change(workspace, window, cx);
        },
    )
    .detach();
}
pub fn open_journal_entry(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let settings = JournalSettings::get_global(cx);
    let journal_dir = match journal_dir(&settings.path) {
        Some(journal_dir) => journal_dir,
        None => {
            log::error!("Can't determine journal directory");
            return;
        }
    };

    let app_state = workspace.app_state().clone();
    workspace.toggle_modal(window, cx, move |window, cx| {
        JournalEntryPicker::new(journal_dir, app_state, window, cx)
    });
}

fn handle_active_item_change(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let settings = JournalSettings::get_global(cx);
    let journal_dir = journal_dir(&settings.path);

    let editor_entity = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx));

    // Handle journal-specific zen mode
    let is_journal_file = editor_entity
        .as_ref()
        .and_then(|editor| {
            editor.read_with(cx, |editor, cx| {
                let buffer = editor.buffer().read(cx);
                buffer
                    .as_singleton()
                    .and_then(|b| b.read(cx).file())
                    .and_then(|f| {
                        if let Some(ref journal_dir_path) = journal_dir {
                            let file_path = f.path().as_std_path();
                            Some(file_path.starts_with(journal_dir_path))
                        } else {
                            None
                        }
                    })
            })
        })
        .unwrap_or(false);

    let state = cx.global::<JournalZenModeState>().clone();

    // If we're in a journal file and zen mode is not active, activate it
    if is_journal_file && !state.is_active {
        activate_zen_mode(window, journal_dir, cx);
    }
    // If we're not in a journal file and zen mode is active, deactivate it
    else if !is_journal_file && state.is_active {
        deactivate_zen_mode(window, cx);
    }
}

fn activate_zen_mode(window: &mut Window, journal_dir: Option<PathBuf>, cx: &mut App) {
    // Store the current profile name before switching
    let previous_profile = cx
        .try_global::<ActiveSettingsProfileName>()
        .map(|p| p.0.clone());

    // Switch to the journal-zen profile if it exists
    // The profile should be defined in the user's settings.json
    cx.set_global(ActiveSettingsProfileName(JOURNAL_ZEN_PROFILE.to_string()));

    // Also dispatch toggle actions for immediate UI changes
    // (These work in conjunction with profile settings)
    window.dispatch_action(workspace::ToggleAllDocks.boxed_clone(), cx);
    window.dispatch_action(workspace::ToggleCenteredLayout.boxed_clone(), cx);
    window.dispatch_action(workspace::ToggleZoom.boxed_clone(), cx);

    let mut state = cx.global::<JournalZenModeState>().clone();
    state.is_active = true;
    state.journal_dir = journal_dir;
    state.previous_profile = previous_profile;
    cx.set_global(state);
}

fn deactivate_zen_mode(window: &mut Window, cx: &mut App) {
    let state = cx.global::<JournalZenModeState>().clone();

    // Restore the previous profile (or clear if there was none)
    if let Some(previous_profile) = &state.previous_profile {
        cx.set_global(ActiveSettingsProfileName(previous_profile.clone()));
    } else {
        // If there was no previous profile, set to empty string (default)
        cx.set_global(ActiveSettingsProfileName(String::new()));
    }

    // Revert toggle actions
    window.dispatch_action(workspace::ToggleAllDocks.boxed_clone(), cx);
    window.dispatch_action(workspace::ToggleCenteredLayout.boxed_clone(), cx);
    window.dispatch_action(workspace::ToggleZoom.boxed_clone(), cx);

    let mut state = cx.global::<JournalZenModeState>().clone();
    state.is_active = false;
    state.journal_dir = None;
    state.previous_profile = None;
    cx.set_global(state);
}

pub fn toggle_journal_zen_mode(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let settings = JournalSettings::get_global(cx);
    let journal_dir = journal_dir(&settings.path);

    // Check if the current active item is a journal file
    let is_journal_file = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
        .and_then(|editor| {
            editor.read_with(cx, |editor, cx| {
                let buffer = editor.buffer().read(cx);
                buffer
                    .as_singleton()
                    .and_then(|b| b.read(cx).file())
                    .and_then(|f| {
                        if let Some(ref journal_dir_path) = journal_dir {
                            let file_path = f.path().as_std_path();
                            Some(file_path.starts_with(journal_dir_path))
                        } else {
                            None
                        }
                    })
            })
        })
        .unwrap_or(false);

    if !is_journal_file {
        log::warn!("Zen mode can only be toggled for journal files");
        return;
    }

    let state = cx.global::<JournalZenModeState>().clone();

    if state.is_active {
        deactivate_zen_mode(window, cx);
    } else {
        activate_zen_mode(window, journal_dir, cx);
    }
}

pub fn focus_section(workspace: &mut Workspace, _window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        return;
    };

    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx);
        let snapshot = buffer.snapshot(cx);

        // Get current cursor position
        let cursor_position = editor.selections.newest_anchor().head();
        let cursor_point = cursor_position.to_point(&snapshot);

        // Find the current section range using tree-sitter outline
        if let Some(section_range) = find_current_section_range(&snapshot, cursor_point.row) {
            // Get the singleton buffer
            if let Some(buffer_entity) = buffer.as_singleton() {
                let buffer_snapshot = buffer_entity.read(cx).snapshot();

                // Create an excerpt range for just this section
                let start_anchor = buffer_snapshot.anchor_before(section_range.start);
                let end_anchor = buffer_snapshot.anchor_after(section_range.end);

                // Clear existing excerpts and add only the current section
                let multibuffer = editor.buffer().clone();
                multibuffer.update(cx, |multibuffer, cx| {
                    multibuffer.clear(cx);
                    multibuffer.push_excerpts(
                        buffer_entity.clone(),
                        [ExcerptRange {
                            context: start_anchor..end_anchor,
                            primary: start_anchor..end_anchor,
                        }],
                        cx,
                    );
                });

                // Trigger reparse to regenerate folds
                buffer_entity.update(cx, |buffer, cx| {
                    buffer.reparse(cx);
                });
            }
        }
    });
}

fn find_current_section_range(
    snapshot: &MultiBufferSnapshot,
    cursor_row: u32,
) -> Option<Range<Point>> {
    let outline = snapshot.outline(None)?;
    if outline.items.is_empty() {
        return None;
    }

    // Find the section that contains the cursor
    let mut current_section_idx: Option<usize> = None;

    for (i, item) in outline.items.iter().enumerate() {
        let start_point = item.range.start.to_point(snapshot);

        // Check if cursor is exactly on this headline
        if cursor_row == start_point.row {
            current_section_idx = Some(i);
            break;
        }

        // Track sections we've passed
        if cursor_row > start_point.row {
            current_section_idx = Some(i);
        } else {
            break;
        }
    }

    let section_idx = current_section_idx?;
    let section = &outline.items[section_idx];

    // The outline item's range already includes all nested content (since we use 'section' nodes)!
    let start_point = section.range.start.to_point(snapshot);
    let end_point = section.range.end.to_point(snapshot);

    Some(start_point..end_point)
}

pub fn unfocus_section(
    workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        return;
    };

    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx);

        // Restore the full buffer view by clearing and re-adding the full excerpt
        if let Some(buffer_entity) = buffer.as_singleton() {
            let buffer_snapshot = buffer_entity.read(cx).snapshot();
            let start_anchor = buffer_snapshot.anchor_before(Point::zero());
            let end_anchor = buffer_snapshot.anchor_after(buffer_snapshot.max_point());

            let multibuffer = editor.buffer().clone();
            multibuffer.update(cx, |multibuffer, cx| {
                multibuffer.clear(cx);
                multibuffer.push_excerpts(
                    buffer_entity.clone(),
                    [ExcerptRange {
                        context: start_anchor..end_anchor,
                        primary: start_anchor..end_anchor,
                    }],
                    cx,
                );
            });

            // Trigger reparse to regenerate folds
            buffer_entity.update(cx, |buffer, cx| {
                buffer.reparse(cx);
            });
        }
    });
}

pub fn cycle_fold(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        return;
    };

    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx);
        let snapshot = buffer.snapshot(cx);

        // Get current cursor position
        let cursor_position = editor.selections.newest_anchor().head();
        let cursor_point = cursor_position.to_point(&snapshot);

        // Get the outline to find sections
        let outline = snapshot.outline(None);
        if outline.is_none() {
            return;
        }
        let outline = outline.unwrap();

        // Find the current section (cursor can be anywhere in the section)
        let mut current_section_idx: Option<usize> = None;
        for (i, item) in outline.items.iter().enumerate() {
            let start_point = item.range.start.to_point(&snapshot);
            if cursor_point.row >= start_point.row {
                current_section_idx = Some(i);
            } else {
                break;
            }
        }

        if current_section_idx.is_none() {
            return;
        }

        let section_idx = current_section_idx.unwrap();
        let section = &outline.items[section_idx];
        let section_start = section.range.start.to_point(&snapshot);
        let section_depth = section.depth;

        // Find the range of the entire section
        let fold_start_row = section_start.row;
        let mut fold_end_row = snapshot.max_point().row;

        for next_item in &outline.items[section_idx + 1..] {
            if next_item.depth <= section_depth {
                fold_end_row = next_item.range.start.to_point(&snapshot).row;
                break;
            }
        }

        // Find direct children (depth = section_depth + 1)
        let mut children: Vec<(usize, u32)> = Vec::new();
        for (i, item) in outline.items[section_idx + 1..].iter().enumerate() {
            let item_depth = item.depth;
            if item_depth <= section_depth {
                break;
            }
            if item_depth == section_depth + 1 {
                let child_start = item.range.start.to_point(&snapshot);
                children.push((section_idx + 1 + i, child_start.row));
            }
        }

        // Check the current fold state
        let display_map = editor.display_map.update(cx, |map, cx| map.snapshot(cx));

        // Determine current state by checking what's folded
        let section_content_folded = if fold_start_row + 1 <= fold_end_row {
            display_map.is_line_folded(multi_buffer::MultiBufferRow(fold_start_row + 1))
        } else {
            false
        };

        let all_children_folded = if !children.is_empty() {
            children.iter().all(|(_, child_row)| {
                if *child_row + 1 <= fold_end_row {
                    display_map.is_line_folded(multi_buffer::MultiBufferRow(*child_row + 1))
                } else {
                    false
                }
            })
        } else {
            false
        };

        // Determine the current state and cycle to the next
        // State 1: FOLDED - the section itself is folded (nothing visible below headline)
        // State 2: CHILDREN - section unfolded, direct children visible but their content folded
        // State 3: SUBTREE - everything visible (nothing folded)

        if section_content_folded {
            // Currently FOLDED → transition to CHILDREN
            // Unfold the main section, but fold all direct children
            editor.unfold_at(multi_buffer::MultiBufferRow(fold_start_row), window, cx);

            // Now fold each direct child using fold_at (which uses the tree-sitter folds)
            for (_, child_row) in &children {
                editor.fold_at(multi_buffer::MultiBufferRow(*child_row), window, cx);
            }
        } else if all_children_folded && !children.is_empty() {
            // Currently CHILDREN → transition to SUBTREE (fully expanded)
            // Unfold all children
            for (_, child_row) in &children {
                editor.unfold_at(multi_buffer::MultiBufferRow(*child_row), window, cx);
            }
        } else {
            // Currently SUBTREE (fully expanded) → transition to FOLDED
            // Fold the entire section
            editor.fold_at(multi_buffer::MultiBufferRow(fold_start_row), window, cx);
        }
    });
}

pub fn cycle_todo_state(
    workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        return;
    };

    let settings = JournalSettings::get_global(cx);
    let todo_keywords = settings.todo_keywords.clone();
    let done_keywords = settings.done_keywords.clone();
    let timestamp_on_done = settings.timestamp_on_done;

    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx);
        let Some(singleton_buffer) = buffer.as_singleton() else {
            log::warn!("Buffer is not a singleton");
            return;
        };

        let buffer_snapshot = singleton_buffer.read(cx).snapshot();

        // Get current cursor position
        let cursor_position = editor.selections.newest_anchor().head();

        // The multi-buffer Anchor contains the text_anchor which is the buffer anchor
        let buffer_anchor = cursor_position.text_anchor;
        let buffer_offset = buffer_anchor.to_offset(&buffer_snapshot);

        // Use tree-sitter to find the headline node at cursor position
        let headline_node = buffer_snapshot.syntax_ancestor(buffer_offset..buffer_offset);

        let Some(mut headline_node) = headline_node else {
            log::info!("No syntax node at cursor position");
            return;
        };

        // Walk up the tree to find a headline node
        while headline_node.kind() != "headline" {
            if let Some(parent) = headline_node.parent() {
                headline_node = parent;
            } else {
                log::info!("Not on a headline");
                return;
            }
        }

        // Find the stars and item nodes within the headline
        let mut stars_node = None;
        let mut item_node = None;
        let mut cursor = headline_node.walk();

        for child in headline_node.children(&mut cursor) {
            match child.kind() {
                "stars" => stars_node = Some(child),
                "item" => item_node = Some(child),
                _ => {}
            }
        }

        let Some(stars_node) = stars_node else {
            log::warn!("Headline missing stars node");
            return;
        };

        let Some(item_node) = item_node else {
            log::warn!("Headline missing item node");
            return;
        };

        // Build all possible keywords (todo + done)
        let mut all_keywords = todo_keywords.clone();
        all_keywords.extend(done_keywords.clone());

        // Check if the item has a TODO keyword - it's the first child expr node
        let mut current_keyword: Option<String> = None;
        let mut keyword_node = None;
        let mut item_cursor = item_node.walk();

        for child in item_node.children(&mut item_cursor) {
            if child.kind() == "expr" {
                let text = buffer_snapshot
                    .text_for_range(child.byte_range())
                    .collect::<String>();
                if all_keywords.iter().any(|kw| kw == &text) {
                    current_keyword = Some(text);
                    keyword_node = Some(child);
                }
                break; // Only check the first expr node
            }
        }

        let (new_keyword, add_timestamp) = if let Some(ref keyword) = current_keyword {
            // Find the next keyword in the cycle
            let current_is_done = done_keywords.contains(keyword);

            // Cycle: TODO keywords → DONE keywords → remove keyword (cycle back to start)
            if current_is_done {
                // If it's a done keyword and it's the last one, remove the keyword entirely
                let current_idx = done_keywords.iter().position(|k| k == keyword).unwrap();
                if current_idx + 1 >= done_keywords.len() {
                    (None, false) // Remove keyword
                } else {
                    (Some(done_keywords[current_idx + 1].clone()), false)
                }
            } else {
                // It's a TODO keyword - cycle through todo keywords first
                let current_idx = todo_keywords.iter().position(|k| k == keyword).unwrap();
                if current_idx + 1 >= todo_keywords.len() {
                    // Move to first DONE keyword
                    if !done_keywords.is_empty() {
                        (Some(done_keywords[0].clone()), timestamp_on_done)
                    } else {
                        (None, false) // No done keywords, remove
                    }
                } else {
                    (Some(todo_keywords[current_idx + 1].clone()), false)
                }
            }
        } else {
            // No keyword found, add the first TODO keyword
            if !todo_keywords.is_empty() {
                (Some(todo_keywords[0].clone()), false)
            } else {
                log::warn!("No TODO keywords configured");
                return;
            }
        };

        // Check if we're transitioning to a DONE state and if there's a recurring task
        let transitioning_to_done = if let Some(ref new_kw) = new_keyword {
            done_keywords.contains(new_kw)
        } else {
            false
        };

        // If transitioning to done, check for recurring timestamps
        if transitioning_to_done {
            // Check the section for SCHEDULED or DEADLINE with repeater
            let headline_end = headline_node.end_byte();
            let buffer_text = buffer_snapshot.text();
            let text_after_headline = &buffer_text[headline_end..];
            let search_end = text_after_headline
                .find("\n*")
                .unwrap_or(text_after_headline.len());
            let section_text = &text_after_headline[..search_end];

            // Look for recurring timestamps
            if let Some((entry_type, current_date, repeater)) = extract_recurring_info(section_text)
            {
                // Clone the headline text for the new task
                let headline_text = buffer_snapshot
                    .text_for_range(headline_node.byte_range())
                    .collect::<String>();

                // Calculate next occurrence
                let next_date = calculate_next_occurrence(
                    current_date,
                    &repeater,
                    Some(Local::now().naive_local().date()),
                );

                // Create new task after applying the edit
                let new_task_headline = create_recurring_task(
                    &headline_text,
                    &todo_keywords[0],
                    next_date,
                    &repeater,
                    entry_type,
                );

                // Get insertion position - after the entire headline section
                let insert_pos = headline_end
                    + section_text
                        .find("\n*")
                        .map(|p| p + 1)
                        .unwrap_or(section_text.len());

                // Apply the status change and insert new task
                singleton_buffer.update(cx, |buffer, cx| {
                    // First, change the current task status
                    let edit_range;
                    let new_text;

                    if let Some(new_kw) = new_keyword.clone() {
                        if let Some(kw_node) = keyword_node {
                            edit_range = kw_node.byte_range();
                            new_text = new_kw.clone();
                        } else {
                            let insert_pos = stars_node.end_byte();
                            edit_range = insert_pos..insert_pos;
                            new_text = format!(" {}", new_kw);
                        }

                        buffer.edit([(edit_range.clone(), new_text)], None, cx);

                        // Add CLOSED timestamp on a separate line if needed
                        if add_timestamp {
                            let now = Local::now();
                            let closed_line =
                                format!("CLOSED: [{}]", now.format("%Y-%m-%d %a %H:%M"));
                            // Insert after the headline
                            buffer.edit(
                                [(headline_end..headline_end, format!("\n{}", closed_line))],
                                None,
                                cx,
                            );
                        }
                    }

                    // Then insert the new recurring task
                    // Note: insert_pos needs to be recalculated if we added a CLOSED line
                    let final_insert_pos = if add_timestamp {
                        // Account for the added CLOSED line
                        insert_pos
                            + format!("\nCLOSED: [{}]", Local::now().format("%Y-%m-%d %a %H:%M"))
                                .len()
                    } else {
                        insert_pos
                    };
                    buffer.edit(
                        [(final_insert_pos..final_insert_pos, new_task_headline)],
                        None,
                        cx,
                    );
                });

                return;
            }
        }

        // Normal cycling without recurring
        let edit_range;
        let new_text;

        if let Some(new_kw) = new_keyword {
            if let Some(kw_node) = keyword_node {
                // Replace existing keyword
                edit_range = kw_node.byte_range();
                new_text = new_kw.clone();
            } else {
                // Insert new keyword after stars
                let insert_pos = stars_node.end_byte();
                edit_range = insert_pos..insert_pos;
                new_text = format!(" {}", new_kw);
            }

            // Apply the keyword change
            singleton_buffer.update(cx, |buffer, cx| {
                buffer.edit([(edit_range.clone(), new_text.clone())], None, cx);
            });

            // Add CLOSED timestamp on a separate line if needed
            if add_timestamp {
                let now = Local::now();
                let closed_line = format!("CLOSED: [{}]", now.format("%Y-%m-%d %a %H:%M"));
                singleton_buffer.update(cx, |buffer, cx| {
                    buffer.edit(
                        [(
                            headline_node.end_byte()..headline_node.end_byte(),
                            format!("\n{}", closed_line),
                        )],
                        None,
                        cx,
                    );
                });
            }

            return;
        } else {
            // Remove keyword (and CLOSED timestamp line if present)
            if let Some(kw_node) = keyword_node {
                // Remove the keyword
                singleton_buffer.update(cx, |buffer, cx| {
                    buffer.edit([(kw_node.byte_range(), String::new())], None, cx);
                });

                // Also check for and remove CLOSED timestamp on the next line
                let headline_end = headline_node.end_byte();
                let buffer_text = buffer_snapshot.text();
                let text_after_headline = &buffer_text[headline_end..];
                let search_end = text_after_headline
                    .find("\n*")
                    .unwrap_or(text_after_headline.len());
                let section_text = &text_after_headline[..search_end];

                // Look for CLOSED line
                if let Some(closed_pos) = section_text.find("CLOSED:") {
                    if let Some(line_end) = section_text[closed_pos..].find('\n') {
                        let closed_line_start = section_text[..closed_pos]
                            .rfind('\n')
                            .map(|p| p)
                            .unwrap_or(0);
                        let closed_line_end = closed_pos + line_end + 1;

                        singleton_buffer.update(cx, |buffer, cx| {
                            buffer.edit(
                                [(
                                    headline_end + closed_line_start
                                        ..headline_end + closed_line_end,
                                    String::new(),
                                )],
                                None,
                                cx,
                            );
                        });
                    } else {
                        // CLOSED is at the end without newline
                        let closed_line_start = section_text[..closed_pos]
                            .rfind('\n')
                            .map(|p| p)
                            .unwrap_or(0);
                        singleton_buffer.update(cx, |buffer, cx| {
                            buffer.edit(
                                [(
                                    headline_end + closed_line_start
                                        ..headline_end + section_text.len(),
                                    String::new(),
                                )],
                                None,
                                cx,
                            );
                        });
                    }
                }
            }
            return;
        };
    });
}

/// Extract recurring info from section text (SCHEDULED/DEADLINE with repeater)
fn extract_recurring_info(
    section_text: &str,
) -> Option<(AgendaEntryType, chrono::NaiveDate, RepeaterInfo)> {
    // Check for SCHEDULED with repeater
    if let Some(scheduled_pos) = section_text.find("SCHEDULED:") {
        let line = &section_text[scheduled_pos..].lines().next()?;
        if let Some((date, repeater)) = parse_timestamp_with_repeater(line) {
            return Some((AgendaEntryType::Scheduled, date, repeater));
        }
    }

    // Check for DEADLINE with repeater
    if let Some(deadline_pos) = section_text.find("DEADLINE:") {
        let line = &section_text[deadline_pos..].lines().next()?;
        if let Some((date, repeater)) = parse_timestamp_with_repeater(line) {
            return Some((AgendaEntryType::Deadline, date, repeater));
        }
    }

    None
}

/// Parse a timestamp line and extract date and repeater info
fn parse_timestamp_with_repeater(line: &str) -> Option<(chrono::NaiveDate, RepeaterInfo)> {
    use regex::Regex;

    // Match patterns like: <2025-01-15 Wed +1w> or <2025-01-15 Wed ++2d> or <2025-01-15 Wed .+1m>
    let re = Regex::new(r"<(\d{4}-\d{2}-\d{2})\s+\w+\s+([\+\.]{1,2})(\d+)([dwmy])>").ok()?;

    if let Some(captures) = re.captures(line) {
        let date_str = captures.get(1)?.as_str();
        let repeater_prefix = captures.get(2)?.as_str();
        let interval_str = captures.get(3)?.as_str();
        let unit_str = captures.get(4)?.as_str();

        let date = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
        let interval = interval_str.parse::<i64>().ok()?;

        let repeater_type = match repeater_prefix {
            "+" => RepeaterType::Cumulative,
            "++" => RepeaterType::CatchUp,
            ".+" => RepeaterType::Restart,
            _ => return None,
        };

        let unit = match unit_str {
            "d" => RepeaterUnit::Day,
            "w" => RepeaterUnit::Week,
            "m" => RepeaterUnit::Month,
            "y" => RepeaterUnit::Year,
            _ => return None,
        };

        Some((
            date,
            RepeaterInfo {
                interval,
                unit,
                repeater_type,
            },
        ))
    } else {
        None
    }
}

/// Create a new recurring task with updated timestamp
fn create_recurring_task(
    original_headline: &str,
    todo_keyword: &str,
    next_date: chrono::NaiveDate,
    repeater: &RepeaterInfo,
    entry_type: AgendaEntryType,
) -> String {
    use regex::Regex;

    // Remove any DONE/CANCELLED keyword
    let re_done = Regex::new(r"(DONE|CANCELLED)\s*").unwrap();
    let mut new_headline = re_done.replace_all(original_headline, "").to_string();

    // Remove CLOSED timestamp line (it's on a separate line)
    let re_closed = Regex::new(r"\n\s*CLOSED:\s*\[.*?\]").unwrap();
    new_headline = re_closed.replace_all(&new_headline, "").to_string();

    // Replace with TODO keyword if not present
    if !new_headline.contains(todo_keyword) {
        // Find the stars and insert TODO after them
        if let Some(stars_end) = new_headline.find('*').and_then(|start| {
            let rest = &new_headline[start..];
            rest.find(|c: char| c != '*' && c != ' ')
                .map(|offset| start + offset)
        }) {
            new_headline.insert_str(stars_end, &format!("{} ", todo_keyword));
        }
    }

    // Build the repeater string
    let repeater_str = format!(
        "{}{}{}",
        match repeater.repeater_type {
            RepeaterType::Cumulative => "+",
            RepeaterType::CatchUp => "++",
            RepeaterType::Restart => ".+",
        },
        repeater.interval,
        match repeater.unit {
            RepeaterUnit::Day => "d",
            RepeaterUnit::Week => "w",
            RepeaterUnit::Month => "m",
            RepeaterUnit::Year => "y",
        }
    );

    let weekday = next_date.format("%a").to_string();
    let timestamp_prefix = match entry_type {
        AgendaEntryType::Scheduled => "SCHEDULED:",
        AgendaEntryType::Deadline => "DEADLINE:",
        _ => "SCHEDULED:",
    };

    // Update the timestamp in the headline text
    let re_timestamp =
        Regex::new(r"(SCHEDULED:|DEADLINE:)\s*<\d{4}-\d{2}-\d{2}\s+\w+\s+[\+\.]{1,2}\d+[dwmy]>")
            .unwrap();

    let new_timestamp = format!(
        "{} <{} {} {}>",
        timestamp_prefix,
        next_date.format("%Y-%m-%d"),
        weekday,
        repeater_str
    );

    if re_timestamp.is_match(&new_headline) {
        new_headline = re_timestamp
            .replace(&new_headline, &new_timestamp)
            .to_string();
    } else {
        // Add timestamp after the headline
        new_headline.push_str(&format!("\n{}", new_timestamp));
    }

    format!("\n{}\n", new_headline)
}

fn format_reminder_duration(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    if secs % (7 * 86400) == 0 {
        format!("{}w", secs / (7 * 86400))
    } else if secs % 86400 == 0 {
        format!("{}d", secs / 86400)
    } else if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

/// Calculate the next occurrence date for a recurring task based on repeater info.
///
/// Repeater types:
/// - Cumulative (+): Shift from the original date by interval
/// - CatchUp (++): Shift from today until the date is in the future
/// - Restart (.+): Shift from the completion date (today)
fn calculate_next_occurrence(
    current_date: chrono::NaiveDate,
    repeater: &RepeaterInfo,
    completion_date: Option<chrono::NaiveDate>,
) -> chrono::NaiveDate {
    use chrono::Datelike;

    let base_date = match repeater.repeater_type {
        RepeaterType::Cumulative => current_date, // Shift from original date
        RepeaterType::CatchUp => current_date,    // Will loop until future
        RepeaterType::Restart => {
            completion_date.unwrap_or_else(|| Local::now().naive_local().date())
        } // From completion
    };

    let mut next_date = match repeater.unit {
        RepeaterUnit::Day => base_date + chrono::Duration::days(repeater.interval),
        RepeaterUnit::Week => base_date + chrono::Duration::weeks(repeater.interval),
        RepeaterUnit::Month => {
            // Handle month arithmetic more carefully
            let mut year = base_date.year();
            let mut month = base_date.month() as i32 + repeater.interval as i32;

            while month > 12 {
                month -= 12;
                year += 1;
            }
            while month < 1 {
                month += 12;
                year -= 1;
            }

            let day = base_date.day().min(days_in_month(year, month as u32));
            chrono::NaiveDate::from_ymd_opt(year, month as u32, day)
                .unwrap_or(base_date + chrono::Duration::days(30 * repeater.interval))
        }
        RepeaterUnit::Year => {
            let new_year = base_date.year() + repeater.interval as i32;
            let month = base_date.month();
            let day = base_date.day().min(days_in_month(new_year, month));
            chrono::NaiveDate::from_ymd_opt(new_year, month, day)
                .unwrap_or(base_date + chrono::Duration::days(365 * repeater.interval))
        }
    };

    // For CatchUp (++), keep adding intervals until the date is in the future
    if repeater.repeater_type == RepeaterType::CatchUp {
        let today = Local::now().naive_local().date();
        while next_date <= today {
            next_date = match repeater.unit {
                RepeaterUnit::Day => next_date + chrono::Duration::days(repeater.interval),
                RepeaterUnit::Week => next_date + chrono::Duration::weeks(repeater.interval),
                RepeaterUnit::Month => {
                    let mut year = next_date.year();
                    let mut month = next_date.month() as i32 + repeater.interval as i32;

                    while month > 12 {
                        month -= 12;
                        year += 1;
                    }

                    let day = next_date.day().min(days_in_month(year, month as u32));
                    chrono::NaiveDate::from_ymd_opt(year, month as u32, day)
                        .unwrap_or(next_date + chrono::Duration::days(30 * repeater.interval))
                }
                RepeaterUnit::Year => {
                    let new_year = next_date.year() + repeater.interval as i32;
                    let month = next_date.month();
                    let day = next_date.day().min(days_in_month(new_year, month));
                    chrono::NaiveDate::from_ymd_opt(new_year, month, day)
                        .unwrap_or(next_date + chrono::Duration::days(365 * repeater.interval))
                }
            };
        }
    }

    next_date
}

/// Helper to get the number of days in a given month
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Reschedule a recurring task by updating its timestamp to the next occurrence.
/// This is called when a task with a repeater is marked DONE.
pub fn reschedule_recurring_task(
    _workspace: &Workspace,
    file_path: &Path,
    line_number: u32,
    current_date: chrono::NaiveDate,
    repeater: &RepeaterInfo,
    entry_type: &AgendaEntryType,
    _cx: &mut Context<Workspace>,
) {
    use std::io::{Read, Seek, Write};

    // Calculate the next occurrence
    let next_date = calculate_next_occurrence(
        current_date,
        repeater,
        Some(Local::now().naive_local().date()),
    );

    // Read the file
    let mut file = match OpenOptions::new().read(true).write(true).open(file_path) {
        Ok(f) => f,
        Err(e) => {
            log::error!("Failed to open file for rescheduling: {}", e);
            return;
        }
    };

    let mut content = String::new();
    if let Err(e) = file.read_to_string(&mut content) {
        log::error!("Failed to read file: {}", e);
        return;
    }

    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();

    // Find the line with the timestamp
    if (line_number as usize) >= lines.len() {
        log::error!("Line number {} out of bounds", line_number);
        return;
    }

    // Look for the timestamp line (might be on the same line or next few lines)
    let start_idx = line_number as usize;
    let end_idx = (start_idx + 5).min(lines.len());

    for i in start_idx..end_idx {
        let line = &lines[i];

        // Check if this line contains the timestamp we need to update
        let prefix = match entry_type {
            AgendaEntryType::Scheduled => "SCHEDULED:",
            AgendaEntryType::Deadline => "DEADLINE:",
            _ => continue,
        };

        if line.contains(prefix) {
            // Format the repeater string
            let repeater_str = format!(
                "{}{}{}",
                match repeater.repeater_type {
                    RepeaterType::Cumulative => "+",
                    RepeaterType::CatchUp => "++",
                    RepeaterType::Restart => ".+",
                },
                repeater.interval,
                match repeater.unit {
                    RepeaterUnit::Day => "d",
                    RepeaterUnit::Week => "w",
                    RepeaterUnit::Month => "m",
                    RepeaterUnit::Year => "y",
                }
            );

            // Get the weekday abbreviation
            let weekday = next_date.format("%a").to_string();

            // Build the new timestamp with repeater
            let new_timestamp = format!(
                "{} <{} {} {}>",
                prefix,
                next_date.format("%Y-%m-%d"),
                weekday,
                repeater_str
            );

            // Replace the line, preserving indentation
            let indent = line
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect::<String>();
            lines[i] = format!("{}{}", indent, new_timestamp);

            // Write the updated content back
            if let Err(e) = file.seek(std::io::SeekFrom::Start(0)) {
                log::error!("Failed to seek file: {}", e);
                return;
            }

            let new_content = lines.join("\n") + "\n";
            if let Err(e) = file.set_len(0) {
                log::error!("Failed to truncate file: {}", e);
                return;
            }
            if let Err(e) = file.write_all(new_content.as_bytes()) {
                log::error!("Failed to write file: {}", e);
                return;
            }

            log::info!(
                "Rescheduled recurring task from {} to {}",
                current_date,
                next_date
            );
            break;
        }
    }
}

pub fn schedule_task(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    log::info!("schedule_task called");
    let Some(_editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        log::info!("No active editor found");
        return;
    };

    // Default to tomorrow as initial date
    let tomorrow = Local::now().naive_local().date() + chrono::Duration::days(1);
    let workspace_weak = cx.entity().downgrade();

    log::info!("Opening date picker modal");
    workspace.toggle_modal(window, cx, |window, cx| {
        DatePickerModal::new(
            tomorrow,
            move |selected_date, reminder_duration, repeater_info, _window, cx| {
                let Some(workspace) = workspace_weak.upgrade() else {
                    return;
                };

                workspace.update(cx, |workspace, cx| {
                    let Some(editor) = workspace
                        .active_item(cx)
                        .and_then(|item| item.act_as::<Editor>(cx))
                    else {
                        return;
                    };

                    let weekday = selected_date.format("%a").to_string();
                    let mut schedule_line = if let Some(repeater) = repeater_info {
                        let repeater_str = format!(
                            "{}{}{}",
                            match repeater.repeater_type {
                                RepeaterType::Cumulative => "+",
                                RepeaterType::CatchUp => "++",
                                RepeaterType::Restart => ".+",
                            },
                            repeater.interval,
                            match repeater.unit {
                                RepeaterUnit::Day => "d",
                                RepeaterUnit::Week => "w",
                                RepeaterUnit::Month => "m",
                                RepeaterUnit::Year => "y",
                            }
                        );
                        format!(
                            "SCHEDULED: <{} {} {}>",
                            selected_date.format("%Y-%m-%d"),
                            weekday,
                            repeater_str
                        )
                    } else {
                        format!(
                            "SCHEDULED: <{} {}>",
                            selected_date.format("%Y-%m-%d"),
                            weekday
                        )
                    };

                    // Add REMIND line if reminder was selected
                    if let Some(duration) = reminder_duration {
                        let remind_str = format_reminder_duration(duration);
                        schedule_line.push_str(&format!("\nREMIND: {}", remind_str));
                    }

                    editor.update(cx, |editor, cx| {
                        let buffer = editor.buffer().read(cx);
                        let Some(singleton_buffer) = buffer.as_singleton() else {
                            return;
                        };

                        let buffer_snapshot = singleton_buffer.read(cx).snapshot();
                        let cursor_position = editor.selections.newest_anchor().head();
                        let buffer_anchor = cursor_position.text_anchor;
                        let buffer_offset = buffer_anchor.to_offset(&buffer_snapshot);

                        // Find the headline node
                        let headline_node =
                            buffer_snapshot.syntax_ancestor(buffer_offset..buffer_offset);
                        let Some(mut headline_node) = headline_node else {
                            return;
                        };

                        while headline_node.kind() != "headline" {
                            if let Some(parent) = headline_node.parent() {
                                headline_node = parent;
                            } else {
                                return;
                            }
                        }

                        // Get the full buffer text to search for existing SCHEDULED
                        let headline_end = headline_node.end_byte();
                        let _headline_start = headline_node.start_byte();
                        let buffer_text = buffer_snapshot.text();

                        // Find the line after the headline where we should look
                        let text_after_headline = &buffer_text[headline_end..];
                        let search_end = text_after_headline
                            .find("\n*")
                            .unwrap_or(text_after_headline.len());
                        let section_text = &text_after_headline[..search_end];

                        singleton_buffer.update(cx, |buffer, cx| {
                            // Look for existing SCHEDULED line in the section
                            if let Some(scheduled_pos) = section_text.find("SCHEDULED:") {
                                // Find the entire line with SCHEDULED
                                let abs_scheduled_start = headline_end + scheduled_pos;
                                let before_scheduled = &section_text[..scheduled_pos];
                                let after_scheduled = &section_text[scheduled_pos..];

                                // Find the start of the line (after the newline, not including it)
                                let line_start = before_scheduled
                                    .rfind('\n')
                                    .map(|pos| headline_end + pos + 1) // +1 to skip the newline
                                    .unwrap_or(headline_end);

                                // Find the end of the line (including the newline)
                                let line_end = after_scheduled
                                    .find('\n')
                                    .map(|pos| abs_scheduled_start + pos + 1)
                                    .unwrap_or(headline_end + section_text.len());

                                // Replace the entire line
                                buffer.edit(
                                    [(line_start..line_end, format!("{}\n", schedule_line))],
                                    None,
                                    cx,
                                );
                            } else {
                                // Insert new SCHEDULED line after the headline with trailing newline
                                buffer.edit(
                                    [(
                                        headline_end..headline_end,
                                        format!("\n{}\n", schedule_line),
                                    )],
                                    None,
                                    cx,
                                );
                            }
                        });
                    });
                });
            },
            window,
            cx,
        )
    });
}

pub fn set_deadline(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(_editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        return;
    };

    // Default to 7 days from now as initial date
    let deadline_date = Local::now().naive_local().date() + chrono::Duration::days(7);
    let workspace_weak = cx.entity().downgrade();

    workspace.toggle_modal(window, cx, |window, cx| {
        DatePickerModal::new(
            deadline_date,
            move |selected_date, reminder_duration, repeater_info, _window, cx| {
                let Some(workspace) = workspace_weak.upgrade() else {
                    return;
                };

                workspace.update(cx, |workspace, cx| {
                    let Some(editor) = workspace
                        .active_item(cx)
                        .and_then(|item| item.act_as::<Editor>(cx))
                    else {
                        return;
                    };

                    let weekday = selected_date.format("%a").to_string();
                    let mut deadline_line = if let Some(repeater) = repeater_info {
                        let repeater_str = format!(
                            "{}{}{}",
                            match repeater.repeater_type {
                                RepeaterType::Cumulative => "+",
                                RepeaterType::CatchUp => "++",
                                RepeaterType::Restart => ".+",
                            },
                            repeater.interval,
                            match repeater.unit {
                                RepeaterUnit::Day => "d",
                                RepeaterUnit::Week => "w",
                                RepeaterUnit::Month => "m",
                                RepeaterUnit::Year => "y",
                            }
                        );
                        format!(
                            "DEADLINE: <{} {} {}>",
                            selected_date.format("%Y-%m-%d"),
                            weekday,
                            repeater_str
                        )
                    } else {
                        format!(
                            "DEADLINE: <{} {}>",
                            selected_date.format("%Y-%m-%d"),
                            weekday
                        )
                    };

                    // Add REMIND line if reminder was selected
                    if let Some(duration) = reminder_duration {
                        let remind_str = format_reminder_duration(duration);
                        deadline_line.push_str(&format!("\nREMIND: {}", remind_str));
                    }

                    editor.update(cx, |editor, cx| {
                        let buffer = editor.buffer().read(cx);
                        let Some(singleton_buffer) = buffer.as_singleton() else {
                            return;
                        };

                        let buffer_snapshot = singleton_buffer.read(cx).snapshot();
                        let cursor_position = editor.selections.newest_anchor().head();
                        let buffer_anchor = cursor_position.text_anchor;
                        let buffer_offset = buffer_anchor.to_offset(&buffer_snapshot);

                        // Find the headline node
                        let headline_node =
                            buffer_snapshot.syntax_ancestor(buffer_offset..buffer_offset);
                        let Some(mut headline_node) = headline_node else {
                            return;
                        };

                        while headline_node.kind() != "headline" {
                            if let Some(parent) = headline_node.parent() {
                                headline_node = parent;
                            } else {
                                return;
                            }
                        }

                        // Get the full buffer text to search for existing DEADLINE
                        let headline_end = headline_node.end_byte();
                        let _headline_start = headline_node.start_byte();
                        let buffer_text = buffer_snapshot.text();

                        // Find the line after the headline where we should look
                        let text_after_headline = &buffer_text[headline_end..];
                        let search_end = text_after_headline
                            .find("\n*")
                            .unwrap_or(text_after_headline.len());
                        let section_text = &text_after_headline[..search_end];

                        singleton_buffer.update(cx, |buffer, cx| {
                            // Look for existing DEADLINE line in the section
                            if let Some(deadline_pos) = section_text.find("DEADLINE:") {
                                // Find the entire line with DEADLINE
                                let abs_deadline_start = headline_end + deadline_pos;
                                let before_deadline = &section_text[..deadline_pos];
                                let after_deadline = &section_text[deadline_pos..];

                                // Find the start of the line (after the newline, not including it)
                                let line_start = before_deadline
                                    .rfind('\n')
                                    .map(|pos| headline_end + pos + 1) // +1 to skip the newline
                                    .unwrap_or(headline_end);

                                // Find the end of the line (including the newline)
                                let line_end = after_deadline
                                    .find('\n')
                                    .map(|pos| abs_deadline_start + pos + 1)
                                    .unwrap_or(headline_end + section_text.len());

                                // Replace the entire line
                                buffer.edit(
                                    [(line_start..line_end, format!("{}\n", deadline_line))],
                                    None,
                                    cx,
                                );
                            } else {
                                // Insert new DEADLINE line after the headline with trailing newline
                                buffer.edit(
                                    [(
                                        headline_end..headline_end,
                                        format!("\n{}\n", deadline_line),
                                    )],
                                    None,
                                    cx,
                                );
                            }
                        });
                    });
                });
            },
            window,
            cx,
        )
    });
}

pub fn set_recurring(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        return;
    };

    // First, find if there's an existing SCHEDULED or DEADLINE timestamp
    let Some((existing_date, existing_type, existing_repeater)) =
        editor.read_with(cx, |editor, cx| {
            let buffer = editor.buffer().read(cx);
            let Some(singleton_buffer) = buffer.as_singleton() else {
                return None;
            };

            let buffer_snapshot = singleton_buffer.read(cx).snapshot();
            let cursor_position = editor.selections.newest_anchor().head();
            let buffer_anchor = cursor_position.text_anchor;
            let buffer_offset = buffer_anchor.to_offset(&buffer_snapshot);

            // Find the headline node
            let headline_node = buffer_snapshot.syntax_ancestor(buffer_offset..buffer_offset)?;
            let mut headline_node = headline_node;

            while headline_node.kind() != "headline" {
                headline_node = headline_node.parent()?;
            }

            // Get the section text after the headline
            let headline_end = headline_node.end_byte();
            let buffer_text = buffer_snapshot.text();
            let text_after_headline = &buffer_text[headline_end..];
            let search_end = text_after_headline
                .find("\n*")
                .unwrap_or(text_after_headline.len());
            let section_text = &text_after_headline[..search_end];

            // Check for existing SCHEDULED or DEADLINE
            if let Some((entry_type, date, repeater)) = extract_recurring_info(section_text) {
                Some((date, entry_type, Some(repeater)))
            } else {
                // Check for non-recurring SCHEDULED or DEADLINE
                use regex::Regex;

                if let Some(scheduled_pos) = section_text.find("SCHEDULED:") {
                    let line = section_text[scheduled_pos..].lines().next()?;
                    let re = Regex::new(r"<(\d{4}-\d{2}-\d{2})\s+\w+>").ok()?;
                    if let Some(captures) = re.captures(line) {
                        let date_str = captures.get(1)?.as_str();
                        let date = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
                        return Some((date, AgendaEntryType::Scheduled, None));
                    }
                }

                if let Some(deadline_pos) = section_text.find("DEADLINE:") {
                    let line = section_text[deadline_pos..].lines().next()?;
                    let re = Regex::new(r"<(\d{4}-\d{2}-\d{2})\s+\w+>").ok()?;
                    if let Some(captures) = re.captures(line) {
                        let date_str = captures.get(1)?.as_str();
                        let date = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
                        return Some((date, AgendaEntryType::Deadline, None));
                    }
                }

                None
            }
        })
    else {
        log::warn!("No SCHEDULED or DEADLINE timestamp found on current task");
        return;
    };

    let workspace_weak = cx.entity().downgrade();
    let entry_type_clone = existing_type.clone();

    workspace.toggle_modal(window, cx, |window, cx| {
        DatePickerModal::new_with_repeater(
            existing_date,
            existing_repeater,
            move |selected_date, _reminder_duration, repeater_info, _window, cx| {
                let entry_type = entry_type_clone.clone();
                let Some(workspace) = workspace_weak.upgrade() else {
                    return;
                };

                workspace.update(cx, |workspace, cx| {
                    let Some(editor) = workspace
                        .active_item(cx)
                        .and_then(|item| item.act_as::<Editor>(cx))
                    else {
                        return;
                    };

                    editor.update(cx, |editor, cx| {
                        let buffer = editor.buffer().read(cx);
                        let Some(singleton_buffer) = buffer.as_singleton() else {
                            return;
                        };

                        let buffer_snapshot = singleton_buffer.read(cx).snapshot();
                        let cursor_position = editor.selections.newest_anchor().head();
                        let buffer_anchor = cursor_position.text_anchor;
                        let buffer_offset = buffer_anchor.to_offset(&buffer_snapshot);

                        let headline_node =
                            buffer_snapshot.syntax_ancestor(buffer_offset..buffer_offset);
                        let Some(mut headline_node) = headline_node else {
                            return;
                        };

                        while headline_node.kind() != "headline" {
                            if let Some(parent) = headline_node.parent() {
                                headline_node = parent;
                            } else {
                                return;
                            }
                        }

                        let headline_end = headline_node.end_byte();
                        let buffer_text = buffer_snapshot.text();
                        let text_after_headline = &buffer_text[headline_end..];
                        let search_end = text_after_headline
                            .find("\n*")
                            .unwrap_or(text_after_headline.len());
                        let section_text = &text_after_headline[..search_end];

                        let weekday = selected_date.format("%a").to_string();
                        let timestamp_prefix = match entry_type {
                            AgendaEntryType::Scheduled => "SCHEDULED:",
                            AgendaEntryType::Deadline => "DEADLINE:",
                            _ => "SCHEDULED:",
                        };

                        let new_timestamp = if let Some(repeater) = repeater_info {
                            let repeater_str = format!(
                                "{}{}{}",
                                match repeater.repeater_type {
                                    RepeaterType::Cumulative => "+",
                                    RepeaterType::CatchUp => "++",
                                    RepeaterType::Restart => ".+",
                                },
                                repeater.interval,
                                match repeater.unit {
                                    RepeaterUnit::Day => "d",
                                    RepeaterUnit::Week => "w",
                                    RepeaterUnit::Month => "m",
                                    RepeaterUnit::Year => "y",
                                }
                            );
                            format!(
                                "{} <{} {} {}>",
                                timestamp_prefix,
                                selected_date.format("%Y-%m-%d"),
                                weekday,
                                repeater_str
                            )
                        } else {
                            format!(
                                "{} <{} {}>",
                                timestamp_prefix,
                                selected_date.format("%Y-%m-%d"),
                                weekday
                            )
                        };

                        singleton_buffer.update(cx, |buffer, cx| {
                            // Find and replace the existing timestamp line
                            let prefix = match entry_type {
                                AgendaEntryType::Scheduled => "SCHEDULED:",
                                AgendaEntryType::Deadline => "DEADLINE:",
                                _ => "SCHEDULED:",
                            };

                            if let Some(timestamp_pos) = section_text.find(prefix) {
                                let abs_timestamp_start = headline_end + timestamp_pos;
                                let before_timestamp = &section_text[..timestamp_pos];
                                let after_timestamp = &section_text[timestamp_pos..];

                                // Find the start of the line (after the newline, not including it)
                                let line_start = before_timestamp
                                    .rfind('\n')
                                    .map(|pos| headline_end + pos + 1) // +1 to skip the newline
                                    .unwrap_or(headline_end);

                                // Find the end of the line (including the newline)
                                let line_end = after_timestamp
                                    .find('\n')
                                    .map(|pos| abs_timestamp_start + pos + 1)
                                    .unwrap_or(headline_end + section_text.len());

                                buffer.edit(
                                    [(line_start..line_end, format!("{}\n", new_timestamp))],
                                    None,
                                    cx,
                                );
                            }
                        });
                    });
                });
            },
            window,
            cx,
        )
    });
}

pub fn open_agenda(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace_weak = cx.entity().downgrade();

    let agenda_view = cx.new(|cx| AgendaView::new(workspace_weak.clone(), cx));

    // Collect items immediately
    agenda_view.update(cx, |view, cx| {
        view.collect_agenda_items(cx);
    });

    workspace.add_item_to_active_pane(Box::new(agenda_view), None, true, window, cx);
}

pub fn capture_template(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let settings = JournalSettings::get_global(cx);

    if settings.capture_templates.is_empty() {
        log::warn!("No capture templates configured");
        return;
    }

    let templates = settings.capture_templates.clone();
    let journal_dir = journal_dir(&settings.path);
    let active_editor = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx));
    let workspace_weak = cx.entity().downgrade();

    workspace.toggle_modal(window, cx, move |window, cx| {
        CaptureTemplatePicker::new(
            templates,
            active_editor,
            journal_dir,
            workspace_weak,
            window,
            cx,
        )
    });
}
pub fn new_journal_entry(workspace: &Workspace, window: &mut Window, cx: &mut App) {
    let settings = JournalSettings::get_global(cx);
    let journal_dir = match journal_dir(&settings.path) {
        Some(journal_dir) => journal_dir,
        None => {
            log::error!("Can't determine journal directory");
            return;
        }
    };
    let journal_dir_clone = journal_dir.clone();

    let now = Local::now();
    let entry_path = journal_dir.join("journal.org");
    let date_heading = format!("* {}", now.format("%Y-%m-%d %A"));
    let time_heading = time_heading(now.time(), &settings.hour_format);

    let create_entry = cx.background_spawn(async move {
        std::fs::create_dir_all(&journal_dir)?;

        // Read existing content
        let existing_content = if entry_path.exists() {
            std::fs::read_to_string(&entry_path).unwrap_or_default()
        } else {
            String::new()
        };

        // Check if today's date heading exists
        let needs_date_heading = !existing_content.contains(&date_heading);

        Ok::<_, std::io::Error>((journal_dir, entry_path, needs_date_heading, date_heading))
    });

    let worktrees = workspace.visible_worktrees(cx).collect::<Vec<_>>();
    let mut open_new_workspace = true;
    'outer: for worktree in worktrees.iter() {
        let worktree_root = worktree.read(cx).abs_path();
        if *worktree_root == journal_dir_clone {
            open_new_workspace = false;
            break;
        }
        for directory in worktree.read(cx).directories(true, 1) {
            let full_directory_path = worktree_root.join(directory.path.as_std_path());
            if full_directory_path.ends_with(&journal_dir_clone) {
                open_new_workspace = false;
                break 'outer;
            }
        }
    }

    let app_state = workspace.app_state().clone();
    let view_snapshot = workspace.weak_handle();

    window
        .spawn(cx, async move |cx| {
            let (journal_dir, entry_path, needs_date_heading, date_heading) = create_entry.await?;
            let opened = if open_new_workspace {
                let (new_workspace, _) = cx
                    .update(|_window, cx| {
                        workspace::open_paths(
                            &[journal_dir],
                            app_state,
                            workspace::OpenOptions::default(),
                            cx,
                        )
                    })?
                    .await?;
                new_workspace
                    .update(cx, |workspace, window, cx| {
                        workspace.open_paths(
                            vec![entry_path],
                            workspace::OpenOptions {
                                visible: Some(OpenVisible::All),
                                ..Default::default()
                            },
                            None,
                            window,
                            cx,
                        )
                    })?
                    .await
            } else {
                view_snapshot
                    .update_in(cx, |workspace, window, cx| {
                        workspace.open_paths(
                            vec![entry_path],
                            workspace::OpenOptions {
                                visible: Some(OpenVisible::All),
                                ..Default::default()
                            },
                            None,
                            window,
                            cx,
                        )
                    })?
                    .await
            };

            if let Some(Some(Ok(item))) = opened.first()
                && let Some(editor) = item.downcast::<Editor>().map(|editor| editor.downgrade())
            {
                editor.update_in(cx, |editor, window, cx| {
                    let len = editor.buffer().read(cx).len(cx);
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |s| s.select_ranges([len..len]),
                    );
                    if len.0 > 0 {
                        editor.insert("\n\n", window, cx);
                    }
                    // Add date heading if needed
                    if needs_date_heading {
                        editor.insert(&date_heading, window, cx);
                        editor.insert("\n\n", window, cx);
                    }
                    // Add time heading
                    editor.insert(&time_heading, window, cx);
                    editor.insert("\n\n", window, cx);
                })?;
            }

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

fn journal_dir(path: &str) -> Option<PathBuf> {
    let expanded = shellexpand::full(path).ok()?;
    let base_path = Path::new(expanded.as_ref());
    let absolute_path = if base_path.is_absolute() {
        base_path.to_path_buf()
    } else {
        log::warn!("Invalid journal path {path:?} (not absolute), falling back to home directory",);
        std::env::home_dir()?
    };
    Some(absolute_path.join("journal"))
}

fn time_heading(now: NaiveTime, hour_format: &HourFormat) -> String {
    match hour_format {
        HourFormat::Hour24 => {
            let hour = now.hour();
            format!("** {}:{:02}", hour, now.minute())
        }
        HourFormat::Hour12 => {
            let (pm, hour) = now.hour12();
            let am_or_pm = if pm { "PM" } else { "AM" };
            format!("** {}:{:02} {}", hour, now.minute(), am_or_pm)
        }
    }
}

fn heading_entry(now: NaiveTime, hour_format: &HourFormat) -> String {
    time_heading(now, hour_format)
}

pub struct JournalEntryPicker {
    picker: Entity<Picker<JournalEntryPickerDelegate>>,
}

#[derive(Clone)]
struct PendingCapture {
    template: CaptureTemplateConfig,
    settings: JournalSettings,
    context: Option<CaptureContext>,
    active_editor: Option<Entity<Editor>>,
    input_text: Option<String>,
    tags: Option<String>,
    workspace: WeakEntity<Workspace>,
    journal_dir: Option<PathBuf>,
}

impl JournalEntryPicker {
    fn new(
        journal_dir: PathBuf,
        app_state: Arc<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate =
            JournalEntryPickerDelegate::new(cx.entity().downgrade(), journal_dir, app_state, cx);

        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }

    fn new_for_capture(
        journal_dir: PathBuf,
        pending_capture: PendingCapture,
        app_state: Arc<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = JournalEntryPickerDelegate::new_for_capture(
            cx.entity().downgrade(),
            journal_dir,
            pending_capture,
            app_state,
            cx,
        );

        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for JournalEntryPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("JournalEntryPicker")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

impl Focusable for JournalEntryPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for JournalEntryPicker {}
impl ModalView for JournalEntryPicker {}

pub struct CaptureTemplatePicker {
    picker: Entity<Picker<CaptureTemplatePickerDelegate>>,
}

#[derive(Clone)]
struct CaptureContext {
    selection: Option<String>,
    file_path: Option<RelPathBuf>,
    line_number: Option<u32>,
}

impl CaptureTemplatePicker {
    fn new(
        templates: Vec<CaptureTemplateConfig>,
        active_editor: Option<Entity<Editor>>,
        journal_dir: Option<PathBuf>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = CaptureTemplatePickerDelegate::new(
            cx.entity().downgrade(),
            templates,
            active_editor,
            journal_dir,
            workspace,
        );

        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for CaptureTemplatePicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CaptureTemplatePicker")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

impl Focusable for CaptureTemplatePicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for CaptureTemplatePicker {}
impl ModalView for CaptureTemplatePicker {}

pub struct CaptureTemplatePickerDelegate {
    picker: WeakEntity<CaptureTemplatePicker>,
    templates: Vec<CaptureTemplateConfig>,
    active_editor: Option<Entity<Editor>>,
    journal_dir: Option<PathBuf>,
    workspace: WeakEntity<Workspace>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl CaptureTemplatePickerDelegate {
    fn new(
        picker: WeakEntity<CaptureTemplatePicker>,
        templates: Vec<CaptureTemplateConfig>,
        active_editor: Option<Entity<Editor>>,
        journal_dir: Option<PathBuf>,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        let candidates = templates
            .iter()
            .enumerate()
            .map(|(id, template)| StringMatchCandidate::new(id, &template.name))
            .collect();

        Self {
            picker,
            templates,
            active_editor,
            journal_dir,
            workspace,
            candidates,
            matches: vec![],
            selected_index: 0,
        }
    }

    /// Scan the journal directory for all tags
    fn scan_tags(journal_dir: &Path) -> Vec<String> {
        use std::collections::HashSet;

        let mut tags = HashSet::new();

        // Recursively scan all markdown files in journal directory
        if let Ok(entries) = std::fs::read_dir(journal_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // Recursively scan subdirectories
                    Self::scan_tags_recursive(&path, &mut tags);
                } else if path.extension().and_then(|s| s.to_str()) == Some("org") {
                    // Scan org-mode file for tags
                    Self::extract_tags_from_file(&path, &mut tags);
                }
            }
        }

        let mut tag_vec: Vec<String> = tags.into_iter().collect();
        tag_vec.sort();
        tag_vec
    }

    fn scan_tags_recursive(dir: &Path, tags: &mut std::collections::HashSet<String>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    Self::scan_tags_recursive(&path, tags);
                } else if path.extension().and_then(|s| s.to_str()) == Some("org") {
                    Self::extract_tags_from_file(&path, tags);
                }
            }
        }
    }

    fn extract_tags_from_file(file_path: &Path, tags: &mut std::collections::HashSet<String>) {
        use std::io::BufRead;

        if let Ok(file) = std::fs::File::open(file_path) {
            let reader = std::io::BufReader::new(file);

            for line in reader.lines().map_while(Result::ok) {
                // Look for tags in format #tag or tags: tag1, tag2
                // Support common tag formats:
                // 1. Hashtags: #tag
                // 2. Tags line: tags: tag1, tag2, tag3
                // 3. Front matter tags

                // Extract hashtags
                for word in line.split_whitespace() {
                    if word.starts_with('#') && word.len() > 1 {
                        let tag = word
                            .trim_start_matches('#')
                            .trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
                        if !tag.is_empty() {
                            tags.insert(tag.to_string());
                        }
                    }
                }

                // Extract from "tags:" or "Tags:" line
                if line.trim().to_lowercase().starts_with("tags:") {
                    let tags_part = line.split(':').nth(1).unwrap_or("");
                    for tag in tags_part.split(',') {
                        let cleaned = tag
                            .trim()
                            .trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
                        if !cleaned.is_empty() {
                            tags.insert(cleaned.to_string());
                        }
                    }
                }
            }
        }
    }

    fn show_tag_picker_and_process(
        template: CaptureTemplateConfig,
        settings: JournalSettings,
        context: Option<CaptureContext>,
        input_text: Option<String>,
        journal_dir: Option<PathBuf>,
        active_editor: Option<Entity<Editor>>,
        workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) {
        let available_tags = Self::scan_tags(&journal_dir.clone().unwrap_or_default());

        log::info!("Tag picker: found {} tags", available_tags.len());

        // Find the window that contains this workspace
        if let Some(_workspace_entity) = workspace.upgrade() {
            let workspace_windows: Vec<_> = cx
                .windows()
                .into_iter()
                .filter_map(|window| window.downcast::<Workspace>())
                .collect();

            log::info!(
                "Tag picker: found {} workspace windows",
                workspace_windows.len()
            );

            // Just use the first workspace window (in practice, this is the one the user is interacting with)
            if let Some(window_handle) = workspace_windows.first() {
                log::info!("Tag picker: opening modal");
                let _ = window_handle.update(cx, |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        TagPickerModal::new(
                            available_tags,
                            move |tags, window, cx| {
                                Self::process_template_with_input(
                                    template.clone(),
                                    settings.clone(),
                                    context.clone(),
                                    input_text.clone(),
                                    Some(tags),
                                    journal_dir.clone(),
                                    active_editor,
                                    window,
                                    cx,
                                );
                            },
                            window,
                            cx,
                        )
                    });
                });
            } else {
                log::error!("Tag picker: no workspace windows found!");
            }
        } else {
            log::error!("Tag picker: workspace entity could not be upgraded!");
        }
    }

    fn process_template_with_input(
        template: CaptureTemplateConfig,
        settings: JournalSettings,
        context: Option<CaptureContext>,
        input_text: Option<String>,
        tags: Option<String>,
        journal_dir: Option<PathBuf>,
        active_editor: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let expanded = Self::expand_template(
            &template.template,
            &settings.hour_format,
            &context,
            input_text.as_deref(),
            tags.as_deref(),
        );

        if let Some(target) = &template.target {
            if let Some(journal_dir) = &journal_dir {
                let target_path = journal_dir.join(target);
                let content = format!("{}\n", expanded);

                cx.background_spawn(async move {
                    if let Some(parent) = target_path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    let mut file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&target_path)
                        .ok()?;
                    std::io::Write::write_all(&mut file, content.as_bytes()).ok()
                })
                .detach();
            }
        } else {
            // If no target specified, try to insert in active editor or open today's journal
            if let Some(editor) = active_editor {
                // Check if it's a journal file
                let is_journal = editor.read_with(cx, |editor, cx| {
                    let buffer = editor.buffer().read(cx);
                    buffer
                        .as_singleton()
                        .and_then(|b| b.read(cx).file())
                        .map(|f| f.path().display(PathStyle::Posix).contains("journal"))
                        .unwrap_or(false)
                });

                if is_journal {
                    // Insert in current journal file
                    let _ = editor.update(cx, |editor, cx| {
                        editor.insert(&expanded, window, cx);
                        editor.insert("\n\n", window, cx);
                    });
                } else if let Some(journal_dir) = &journal_dir {
                    // Open today's journal and insert
                    let target_path = journal_dir.join("journal.org");
                    let content = format!("{}\n\n", expanded);

                    cx.background_spawn(async move {
                        if let Some(parent) = target_path.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        let mut file = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&target_path)
                            .ok()?;
                        std::io::Write::write_all(&mut file, content.as_bytes()).ok()
                    })
                    .detach();
                }
            } else if let Some(journal_dir) = &journal_dir {
                // No editor active, append to today's journal
                let target_path = journal_dir.join("journal.org");
                let content = format!("{}\n\n", expanded);

                cx.background_spawn(async move {
                    if let Some(parent) = target_path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    let mut file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&target_path)
                        .ok()?;
                    std::io::Write::write_all(&mut file, content.as_bytes()).ok()
                })
                .detach();
            }
        }
    }

    fn expand_template(
        template: &str,
        hour_format: &HourFormat,
        context: &Option<CaptureContext>,
        input: Option<&str>,
        tags: Option<&str>,
    ) -> String {
        let now = Local::now();
        let time = now.time();
        let date = now.format("%Y-%m-%d").to_string();

        let time_str = match hour_format {
            HourFormat::Hour24 => {
                format!("{}:{:02}", time.hour(), time.minute())
            }
            HourFormat::Hour12 => {
                let (pm, hour) = time.hour12();
                let am_or_pm = if pm { "PM" } else { "AM" };
                format!("{}:{:02} {}", hour, time.minute(), am_or_pm)
            }
        };

        let mut result = template
            .replace("{date}", &date)
            .replace("{time}", &time_str)
            .replace("{cursor}", "");

        if let Some(input_text) = input {
            result = result.replace("{input}", input_text);
        }

        if let Some(tags_text) = tags {
            result = result.replace("{tags}", tags_text);
        }

        if let Some(ctx) = context {
            if let Some(selection) = &ctx.selection {
                result = result.replace("{selection}", selection);
            }

            if let Some(file_path) = &ctx.file_path {
                let file_name = file_path.file_name().unwrap_or("");
                result = result.replace("{file}", file_name);

                if let Some(line) = ctx.line_number {
                    // Create a markdown link format with full relative path
                    let path_str = file_path.display(PathStyle::Posix);
                    let display_text = format!("{}:{}", path_str, line);
                    // Use the full relative path in both display and link
                    let link = format!("[{}]({}:{})", display_text, path_str, line);
                    result = result.replace("{link}", &link);
                }
            }
        }

        result
    }
}

impl PickerDelegate for CaptureTemplatePickerDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a capture template…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(mat) = self.matches.get(self.selected_index) {
            let template = self.templates[mat.candidate_id].clone();
            let settings = JournalSettings::get_global(cx).clone();
            let journal_dir = self.journal_dir.clone();
            let active_editor = self.active_editor.clone();

            // Extract context from editor if available
            let context = active_editor.as_ref().and_then(|editor| {
                editor.read_with(cx, |editor_read, cx| {
                    let selection = editor_read.selections.newest_anchor().clone();
                    let buffer = editor_read.buffer().read(cx);
                    let buffer_snapshot = buffer.snapshot(cx);

                    let selection_text = if selection.start != selection.end {
                        let range = selection.range();
                        let start = range.start.to_offset(&buffer_snapshot);
                        let end = range.end.to_offset(&buffer_snapshot);
                        Some(
                            buffer_snapshot
                                .text_for_range(start..end)
                                .collect::<String>(),
                        )
                    } else {
                        None
                    };

                    // Get file path from buffer
                    let cursor_position = selection.head().to_point(&buffer_snapshot);
                    let file_path = buffer
                        .as_singleton()
                        .and_then(|b| b.read(cx).file())
                        .map(|f| f.path().to_rel_path_buf());
                    let line_number = Some(cursor_position.row + 1);

                    Some(CaptureContext {
                        selection: selection_text,
                        file_path,
                        line_number,
                    })
                })
            });

            // Dismiss the picker first
            self.dismissed(window, cx);

            // Check if we need to prompt for target file
            if template.target.is_none() {
                // Show journal entry picker for target selection
                if let Some(workspace) = self.workspace.upgrade() {
                    if let Some(journal_dir_path) = journal_dir.clone() {
                        let pending = PendingCapture {
                            template: template.clone(),
                            settings: settings.clone(),
                            context: context.clone(),
                            active_editor: active_editor.clone(),
                            input_text: None,
                            tags: None,
                            workspace: self.workspace.clone(),
                            journal_dir: journal_dir.clone(),
                        };

                        workspace.update(cx, |workspace, cx| {
                            let app_state = workspace.app_state().clone();
                            workspace.toggle_modal(window, cx, move |window, cx| {
                                JournalEntryPicker::new_for_capture(
                                    journal_dir_path,
                                    pending,
                                    app_state,
                                    window,
                                    cx,
                                )
                            });
                        });
                    }
                }
            } else if template.prompt_for_input.unwrap_or(false) {
                // Get the workspace to show the input modal
                if let Some(workspace_entity) = self.workspace.upgrade() {
                    let input_prompt = template
                        .input_prompt
                        .clone()
                        .unwrap_or_else(|| "Enter text:".to_string());
                    let prompt_for_tags = template.prompt_for_tags.unwrap_or(false);
                    let workspace_weak = self.workspace.clone();

                    workspace_entity.update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, move |window, cx| {
                            TextInputModal::new(
                                input_prompt,
                                move |input_text, window, cx| {
                                    // Check if we need to prompt for tags next
                                    if prompt_for_tags {
                                        // Defer showing tag picker to avoid updating TextInputModal while it's being dismissed
                                        cx.defer(move |cx| {
                                            Self::show_tag_picker_and_process(
                                                template.clone(),
                                                settings.clone(),
                                                context.clone(),
                                                Some(input_text),
                                                journal_dir.clone(),
                                                active_editor.clone(),
                                                workspace_weak,
                                                cx,
                                            );
                                        });
                                    } else {
                                        // Process the template with just the input
                                        Self::process_template_with_input(
                                            template.clone(),
                                            settings.clone(),
                                            context.clone(),
                                            Some(input_text),
                                            None,
                                            journal_dir.clone(),
                                            active_editor.clone(),
                                            window,
                                            cx,
                                        );
                                    }
                                },
                                window,
                                cx,
                            )
                        });
                    });
                }
            } else if template.prompt_for_tags.unwrap_or(false) {
                // Only prompt for tags (no input)
                Self::show_tag_picker_and_process(
                    template.clone(),
                    settings.clone(),
                    context.clone(),
                    None,
                    journal_dir.clone(),
                    active_editor.clone(),
                    self.workspace.clone(),
                    cx,
                );
            } else {
                // Process the template without input or tags
                Self::process_template_with_input(
                    template,
                    settings.clone(),
                    context,
                    None,
                    None,
                    journal_dir,
                    active_editor,
                    window,
                    cx,
                );
            }
        } else {
            self.dismissed(window, cx);
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };

            this.update(cx, |this, cx| {
                let delegate = &mut this.delegate;
                delegate.matches = matches;
                delegate.selected_index = delegate
                    .selected_index
                    .min(delegate.matches.len().saturating_sub(1));
                cx.notify();
            })
            .log_err();
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let template = &self.templates[mat.candidate_id];

        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(HighlightedLabel::new(
                mat.string.clone(),
                mat.positions.clone(),
            ));

        if let Some(description) = &template.description {
            Some(item.end_slot::<Label>(Label::new(description.clone()).color(Color::Muted)))
        } else {
            Some(item)
        }
    }
}

pub struct JournalEntryPickerDelegate {
    picker: WeakEntity<JournalEntryPicker>,
    journal_dir: PathBuf,
    app_state: Arc<AppState>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    capture_mode: bool,
    pending_capture: Option<PendingCapture>,
}

impl JournalEntryPickerDelegate {
    fn new(
        picker: WeakEntity<JournalEntryPicker>,
        journal_dir: PathBuf,
        app_state: Arc<AppState>,
        _cx: &mut Context<JournalEntryPicker>,
    ) -> Self {
        let candidates = Self::collect_journal_entries(&journal_dir);

        Self {
            picker,
            journal_dir,
            app_state,
            candidates,
            matches: vec![],
            selected_index: 0,
            capture_mode: false,
            pending_capture: None,
        }
    }

    fn new_for_capture(
        picker: WeakEntity<JournalEntryPicker>,
        journal_dir: PathBuf,
        pending_capture: PendingCapture,
        app_state: Arc<AppState>,
        _cx: &mut Context<JournalEntryPicker>,
    ) -> Self {
        let candidates = Self::collect_journal_entries(&journal_dir);

        Self {
            picker,
            journal_dir,
            app_state,
            candidates,
            matches: vec![],
            selected_index: 0,
            capture_mode: true,
            pending_capture: Some(pending_capture),
        }
    }

    fn collect_journal_entries(journal_dir: &Path) -> Vec<StringMatchCandidate> {
        let mut entries = Vec::new();
        let mut candidate_id = 0;

        if let Ok(year_dirs) = std::fs::read_dir(journal_dir) {
            for year_entry in year_dirs.flatten() {
                if let Ok(month_dirs) = std::fs::read_dir(year_entry.path()) {
                    for month_entry in month_dirs.flatten() {
                        if let Ok(day_files) = std::fs::read_dir(month_entry.path()) {
                            for day_file in day_files.flatten() {
                                let path = day_file.path();
                                if path.extension().and_then(|s| s.to_str()) == Some("org") {
                                    if let Some(display_name) = Self::format_entry_name(&path) {
                                        entries.push(StringMatchCandidate::new(
                                            candidate_id,
                                            &display_name,
                                        ));
                                        candidate_id += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        entries.sort_by(|a, b| b.string.cmp(&a.string));
        entries
    }

    fn format_entry_name(path: &Path) -> Option<String> {
        let components: Vec<_> = path.components().rev().take(3).collect();
        if components.len() >= 3 {
            let day = components[0].as_os_str().to_str()?.trim_end_matches(".org");
            let month = components[1].as_os_str().to_str()?;
            let year = components[2].as_os_str().to_str()?;
            Some(format!("{}/{}/{}", year, month, day))
        } else {
            None
        }
    }

    fn get_entry_path(&self, mat: &StringMatch) -> Option<PathBuf> {
        let display_name = &mat.string;
        let parts: Vec<&str> = display_name.split('/').collect();
        if parts.len() == 3 {
            let path = self
                .journal_dir
                .join(parts[0])
                .join(parts[1])
                .join(format!("{}.org", parts[2]));
            Some(path)
        } else {
            None
        }
    }
}

impl PickerDelegate for JournalEntryPickerDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a journal entry…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(mat) = self.matches.get(self.selected_index) {
            if let Some(entry_path) = self.get_entry_path(mat) {
                if self.capture_mode {
                    // In capture mode, process the template with the selected file
                    if let Some(pending) = self.pending_capture.take() {
                        let relative_path = entry_path
                            .strip_prefix(&self.journal_dir)
                            .unwrap_or(&entry_path)
                            .to_string_lossy()
                            .to_string();

                        let mut template = pending.template;
                        template.target = Some(relative_path);

                        // Check if we need input
                        if template.prompt_for_input.unwrap_or(false)
                            && pending.input_text.is_none()
                        {
                            // Show input modal after file selection
                            if let Some(workspace) = pending.workspace.upgrade() {
                                let input_prompt = template
                                    .input_prompt
                                    .clone()
                                    .unwrap_or_else(|| "Enter text:".to_string());

                                let settings = pending.settings.clone();
                                let context = pending.context.clone();
                                let journal_dir = pending.journal_dir.clone();
                                let active_editor = pending.active_editor;

                                workspace.update(cx, |workspace, cx| {
                                    workspace.toggle_modal(window, cx, move |window, cx| {
                                        TextInputModal::new(
                                            input_prompt,
                                            move |input_text, window, cx| {
                                                CaptureTemplatePickerDelegate::process_template_with_input(
                                                    template.clone(),
                                                    settings.clone(),
                                                    context.clone(),
                                                    Some(input_text),
                                                    None,
                                                    journal_dir.clone(),
                                                    active_editor,
                                                    window,
                                                    cx,
                                                );
                                            },
                                            window,
                                            cx,
                                        )
                                    });
                                });
                                return;
                            }
                        }

                        CaptureTemplatePickerDelegate::process_template_with_input(
                            template,
                            pending.settings,
                            pending.context,
                            pending.input_text,
                            pending.tags,
                            pending.journal_dir,
                            pending.active_editor,
                            window,
                            cx,
                        );
                    }
                } else {
                    // Normal mode - open the file
                    let app_state = self.app_state.clone();
                    window
                        .spawn(cx, async move |cx| {
                            cx.update(|_window, cx| {
                                workspace::open_paths(
                                    &[entry_path],
                                    app_state,
                                    workspace::OpenOptions {
                                        visible: Some(OpenVisible::All),
                                        ..Default::default()
                                    },
                                    cx,
                                )
                            })?
                            .await?;
                            anyhow::Ok(())
                        })
                        .detach_and_log_err(cx);
                }
            }
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };

            this.update(cx, |this, cx| {
                let delegate = &mut this.delegate;
                delegate.matches = matches;
                delegate.selected_index = delegate
                    .selected_index
                    .min(delegate.matches.len().saturating_sub(1));
                cx.notify();
            })
            .log_err();
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    mat.string.clone(),
                    mat.positions.clone(),
                )),
        )
    }
}

/// Text input modal for capture templates
pub struct TextInputModal {
    prompt: String,
    input_editor: Entity<Editor>,
    on_confirm: Option<Box<dyn FnOnce(String, &mut Window, &mut App) + 'static>>,
    _subscriptions: Vec<Subscription>,
}

/// Tag selection modal for capture templates
pub struct TagPickerModal {
    picker: Entity<Picker<TagPickerDelegate>>,
}

pub struct TagPickerDelegate {
    _picker: WeakEntity<Picker<Self>>,
    available_tags: Vec<String>,
    selected_tags: Vec<String>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    on_confirm: Option<Box<dyn FnOnce(String, &mut Window, &mut App) + 'static>>,
}

impl TextInputModal {
    pub fn new(
        prompt: String,
        on_confirm: impl FnOnce(String, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Enter text...", window, cx);
            editor
        });

        let focus_handle = input_editor.focus_handle(cx);
        focus_handle.focus(window);

        let subscriptions = vec![cx.subscribe_in(&input_editor, window, Self::on_editor_event)];

        Self {
            prompt,
            input_editor,
            on_confirm: Some(Box::new(on_confirm)),
            _subscriptions: subscriptions,
        }
    }

    fn on_editor_event(
        &mut self,
        _: &Entity<Editor>,
        event: &editor::EditorEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            editor::EditorEvent::Blurred => cx.emit(DismissEvent),
            _ => {}
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(on_confirm) = self.on_confirm.take() {
            let input = self.input_editor.read(cx).text(cx);
            cx.emit(DismissEvent);
            on_confirm(input, window, cx);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Render for TextInputModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("TextInputModal")
            .w(rems(34.))
            .elevation_2(cx)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                v_flex()
                    .p_4()
                    .gap_2()
                    .child(Label::new(self.prompt.clone()).size(LabelSize::Default))
                    .child(
                        div()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .rounded_md()
                            .px_2()
                            .py_1()
                            .child(self.input_editor.clone()),
                    ),
            )
    }
}

impl Focusable for TextInputModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input_editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for TextInputModal {}
impl ModalView for TextInputModal {}

impl TagPickerModal {
    pub fn new(
        available_tags: Vec<String>,
        on_confirm: impl FnOnce(String, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = TagPickerDelegate::new(available_tags, on_confirm);
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for TagPickerModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl Focusable for TagPickerModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for TagPickerModal {}
impl ModalView for TagPickerModal {}

impl TagPickerDelegate {
    fn new(
        available_tags: Vec<String>,
        on_confirm: impl FnOnce(String, &mut Window, &mut App) + 'static,
    ) -> Self {
        let candidates = available_tags
            .iter()
            .enumerate()
            .map(|(id, tag)| StringMatchCandidate::new(id, tag.as_str()))
            .collect();

        let matches = available_tags
            .iter()
            .enumerate()
            .map(|(id, _)| StringMatch {
                candidate_id: id,
                score: 0.0,
                positions: Vec::new(),
                string: available_tags[id].clone(),
            })
            .collect();

        Self {
            _picker: WeakEntity::new_invalid(),
            available_tags,
            selected_tags: Vec::new(),
            candidates,
            matches,
            selected_index: 0,
            on_confirm: Some(Box::new(on_confirm)),
        }
    }
}

impl PickerDelegate for TagPickerDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search tags...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        // Secondary confirm (Cmd+Enter): always finalize
        // Primary confirm with no match but tags selected: also finalize
        let has_match = self.matches.get(self.selected_index).is_some();
        let should_finalize = secondary || (!has_match && !self.selected_tags.is_empty());

        if should_finalize {
            if let Some(on_confirm) = self.on_confirm.take() {
                // Add # prefix to each tag and join with spaces
                let tags = self
                    .selected_tags
                    .iter()
                    .map(|tag| {
                        if tag.starts_with('#') {
                            tag.clone()
                        } else {
                            format!("#{}", tag)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                self.dismissed(window, cx);
                on_confirm(tags, window, cx);
            }
            return;
        }

        // Primary Enter: toggle tag selection
        if let Some(mat) = self.matches.get(self.selected_index) {
            let tag = &self.available_tags[mat.candidate_id];

            // Toggle tag selection
            if let Some(pos) = self.selected_tags.iter().position(|t| t == tag) {
                self.selected_tags.remove(pos);
            } else {
                self.selected_tags.push(tag.clone());
            }

            // Update the picker to show selection
            cx.notify();
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let candidates = self.candidates.clone();
        let executor = cx.background_executor().clone();

        let background = executor.clone().spawn(async move {
            if query.is_empty() {
                // Show all tags when no query
                (0..candidates.len())
                    .map(|id| StringMatch {
                        candidate_id: id,
                        score: 0.0,
                        positions: Vec::new(),
                        string: candidates[id].string.clone(),
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    false,
                    usize::MAX,
                    &Default::default(),
                    executor.clone(),
                )
                .await
            }
        });

        cx.spawn_in(window, async move |this, cx| {
            let matches = background.await;
            let _ = this.update(cx, |this, cx| {
                this.delegate.matches = matches;
                this.delegate.selected_index = 0;
                cx.notify();
            });
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let tag = &self.available_tags[mat.candidate_id];
        let is_selected = self.selected_tags.contains(tag);

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .gap_2()
                        .when(is_selected, |this| {
                            this.child(Label::new("✓").color(Color::Success))
                        })
                        .child(HighlightedLabel::new(
                            format!("#{}", tag),
                            mat.positions.clone(),
                        )),
                ),
        )
    }
}

// Date picker modal for scheduling and deadlines
pub struct DatePickerModal {
    selected_date: chrono::NaiveDate,
    selected_reminder: Option<std::time::Duration>,
    selected_repeater: Option<RepeaterInfo>,
    on_confirm: Option<
        Box<
            dyn FnOnce(
                chrono::NaiveDate,
                Option<std::time::Duration>,
                Option<RepeaterInfo>,
                &mut Window,
                &mut App,
            ),
        >,
    >,
    focus_handle: FocusHandle,
}

impl DatePickerModal {
    pub fn new(
        initial_date: chrono::NaiveDate,
        on_confirm: impl FnOnce(
            chrono::NaiveDate,
            Option<std::time::Duration>,
            Option<RepeaterInfo>,
            &mut Window,
            &mut App,
        ) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);

        Self {
            selected_date: initial_date,
            selected_reminder: None,
            selected_repeater: None,
            on_confirm: Some(Box::new(on_confirm)),
            focus_handle,
        }
    }

    pub fn new_with_repeater(
        initial_date: chrono::NaiveDate,
        initial_repeater: Option<RepeaterInfo>,
        on_confirm: impl FnOnce(
            chrono::NaiveDate,
            Option<std::time::Duration>,
            Option<RepeaterInfo>,
            &mut Window,
            &mut App,
        ) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);

        Self {
            selected_date: initial_date,
            selected_reminder: None,
            selected_repeater: initial_repeater,
            on_confirm: Some(Box::new(on_confirm)),
            focus_handle,
        }
    }

    fn cycle_reminder(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // Cycle through: None -> 10m -> 1h -> 1d -> 1w -> None
        self.selected_reminder = match self.selected_reminder {
            None => Some(std::time::Duration::from_secs(10 * 60)), // 10 minutes
            Some(d) if d.as_secs() == 10 * 60 => Some(std::time::Duration::from_secs(3600)), // 1 hour
            Some(d) if d.as_secs() == 3600 => Some(std::time::Duration::from_secs(86400)), // 1 day
            Some(d) if d.as_secs() == 86400 => Some(std::time::Duration::from_secs(7 * 86400)), // 1 week
            _ => None,
        };
        cx.notify();
    }

    fn cycle_repeater(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // Cycle through common recurring patterns
        // None -> +1d -> +1w -> +2w -> +1m -> +3m -> +1y -> ++1d -> ++1w -> .+1d -> .+1w -> None
        self.selected_repeater = match &self.selected_repeater {
            None => Some(RepeaterInfo {
                interval: 1,
                unit: RepeaterUnit::Day,
                repeater_type: RepeaterType::Cumulative,
            }),
            Some(r) if r.repeater_type == RepeaterType::Cumulative => match (&r.unit, r.interval) {
                (RepeaterUnit::Day, 1) => Some(RepeaterInfo {
                    interval: 1,
                    unit: RepeaterUnit::Week,
                    repeater_type: RepeaterType::Cumulative,
                }),
                (RepeaterUnit::Week, 1) => Some(RepeaterInfo {
                    interval: 2,
                    unit: RepeaterUnit::Week,
                    repeater_type: RepeaterType::Cumulative,
                }),
                (RepeaterUnit::Week, 2) => Some(RepeaterInfo {
                    interval: 1,
                    unit: RepeaterUnit::Month,
                    repeater_type: RepeaterType::Cumulative,
                }),
                (RepeaterUnit::Month, 1) => Some(RepeaterInfo {
                    interval: 3,
                    unit: RepeaterUnit::Month,
                    repeater_type: RepeaterType::Cumulative,
                }),
                (RepeaterUnit::Month, 3) => Some(RepeaterInfo {
                    interval: 1,
                    unit: RepeaterUnit::Year,
                    repeater_type: RepeaterType::Cumulative,
                }),
                _ => Some(RepeaterInfo {
                    interval: 1,
                    unit: RepeaterUnit::Day,
                    repeater_type: RepeaterType::CatchUp,
                }),
            },
            Some(r) if r.repeater_type == RepeaterType::CatchUp => match (&r.unit, r.interval) {
                (RepeaterUnit::Day, 1) => Some(RepeaterInfo {
                    interval: 1,
                    unit: RepeaterUnit::Week,
                    repeater_type: RepeaterType::CatchUp,
                }),
                _ => Some(RepeaterInfo {
                    interval: 1,
                    unit: RepeaterUnit::Day,
                    repeater_type: RepeaterType::Restart,
                }),
            },
            Some(r) if r.repeater_type == RepeaterType::Restart => match (&r.unit, r.interval) {
                (RepeaterUnit::Day, 1) => Some(RepeaterInfo {
                    interval: 1,
                    unit: RepeaterUnit::Week,
                    repeater_type: RepeaterType::Restart,
                }),
                _ => None,
            },
            _ => None,
        };
        cx.notify();
    }

    fn adjust_date(&mut self, days: i64, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(new_date) = self
            .selected_date
            .checked_add_signed(chrono::Duration::days(days))
        {
            self.selected_date = new_date;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(on_confirm) = self.on_confirm.take() {
            on_confirm(
                self.selected_date,
                self.selected_reminder,
                self.selected_repeater.clone(),
                window,
                cx,
            );
            cx.emit(DismissEvent);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn next_day(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.adjust_date(1, window, cx);
    }

    fn prev_day(&mut self, _: &menu::SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.adjust_date(-1, window, cx);
    }

    fn next_week(&mut self, _: &DatePickerNextWeek, window: &mut Window, cx: &mut Context<Self>) {
        self.adjust_date(7, window, cx);
    }

    fn prev_week(
        &mut self,
        _: &DatePickerPreviousWeek,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adjust_date(-7, window, cx);
    }

    fn cycle_reminder_action(
        &mut self,
        _: &DatePickerCycleReminder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_reminder(window, cx);
    }

    fn cycle_repeater_action(
        &mut self,
        _: &DatePickerCycleRepeater,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_repeater(window, cx);
    }
}

impl Render for DatePickerModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weekday = self.selected_date.format("%A").to_string();
        let formatted_date = self.selected_date.format("%Y-%m-%d").to_string();

        let reminder_text = match self.selected_reminder {
            None => "None".to_string(),
            Some(d) => {
                let secs = d.as_secs();
                if secs == 10 * 60 {
                    "10 minutes".to_string()
                } else if secs == 3600 {
                    "1 hour".to_string()
                } else if secs == 86400 {
                    "1 day".to_string()
                } else if secs == 7 * 86400 {
                    "1 week".to_string()
                } else {
                    format!("{}s", secs)
                }
            }
        };

        let repeater_text = match &self.selected_repeater {
            None => "None".to_string(),
            Some(r) => {
                let prefix = match r.repeater_type {
                    RepeaterType::Cumulative => "+",
                    RepeaterType::CatchUp => "++",
                    RepeaterType::Restart => ".+",
                };
                let unit = match r.unit {
                    RepeaterUnit::Day => "day",
                    RepeaterUnit::Week => "week",
                    RepeaterUnit::Month => "month",
                    RepeaterUnit::Year => "year",
                };
                let unit_plural = if r.interval == 1 {
                    unit
                } else {
                    match r.unit {
                        RepeaterUnit::Day => "days",
                        RepeaterUnit::Week => "weeks",
                        RepeaterUnit::Month => "months",
                        RepeaterUnit::Year => "years",
                    }
                };
                format!(
                    "{}{} {} ({})",
                    prefix,
                    r.interval,
                    unit_plural,
                    match r.repeater_type {
                        RepeaterType::Cumulative => "from date",
                        RepeaterType::CatchUp => "catch up",
                        RepeaterType::Restart => "from completion",
                    }
                )
            }
        };

        v_flex()
            .key_context("DatePickerModal")
            .track_focus(&self.focus_handle)
            .w(rems(34.))
            .elevation_2(cx)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::prev_day))
            .on_action(cx.listener(Self::next_day))
            .on_action(cx.listener(Self::prev_week))
            .on_action(cx.listener(Self::next_week))
            .on_action(cx.listener(Self::cycle_reminder_action))
            .on_action(cx.listener(Self::cycle_repeater_action))
            .p_4()
            .gap_2()
            .child(
                h_flex()
                    .justify_center()
                    .child(Label::new("Select Date").size(LabelSize::Large)),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .justify_center()
                            .gap_2()
                            .child(Label::new(weekday).size(LabelSize::Default)),
                    )
                    .child(
                        h_flex()
                            .justify_center()
                            .gap_2()
                            .child(Label::new(formatted_date).size(LabelSize::Large)),
                    ),
            )
            .child(
                h_flex().justify_center().gap_2().mt_4().child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(Label::new("Reminder:").size(LabelSize::Small))
                                .child(Label::new(reminder_text).size(LabelSize::Small).color(
                                    if self.selected_reminder.is_some() {
                                        Color::Accent
                                    } else {
                                        Color::Muted
                                    },
                                )),
                        )
                        .child(
                            Label::new("Press 'r' to cycle reminder options")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                ),
            )
            .child(
                h_flex().justify_center().gap_2().mt_2().child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(Label::new("Recurring:").size(LabelSize::Small))
                                .child(Label::new(repeater_text).size(LabelSize::Small).color(
                                    if self.selected_repeater.is_some() {
                                        Color::Accent
                                    } else {
                                        Color::Muted
                                    },
                                )),
                        )
                        .child(
                            Label::new("Press 'p' to cycle repeater options")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                ),
            )
            .child(
                h_flex().justify_center().gap_2().mt_2().child(
                    Label::new("Use ↑/↓ for ±1 day, ←/→ for ±7 days")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .child(
                h_flex().justify_center().gap_2().child(
                    Label::new("Press Enter to confirm, Esc to cancel")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
    }
}

impl Focusable for DatePickerModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for DatePickerModal {}
impl ModalView for DatePickerModal {}

// Agenda view for scheduled and deadline tasks
#[derive(Debug, Clone)]
pub struct AgendaItem {
    pub file_path: PathBuf,
    pub headline: String,
    pub date: chrono::NaiveDate,
    pub entry_type: AgendaEntryType,
    pub line_number: u32,
    pub tags: Vec<String>,
    pub todo_keyword: Option<String>,
    pub warning_days: Option<i64>, // Number of days before deadline to warn (e.g., -5d means 5 days before)
    pub reminder_duration: Option<std::time::Duration>, // Duration before deadline to remind (e.g., REMIND: 10m, 1d)
    pub repeater: Option<RepeaterInfo>, // Repeater interval for recurring tasks (e.g., +1w, .+1d)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepeaterInfo {
    pub interval: i64,               // Number of units (e.g., 1, 2, 7)
    pub unit: RepeaterUnit,          // Unit type (day, week, month, year)
    pub repeater_type: RepeaterType, // Type of repeater (+, ++, .+)
}

#[derive(Debug, Clone, PartialEq)]
pub enum RepeaterUnit {
    Day,
    Week,
    Month,
    Year,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RepeaterType {
    Cumulative, // + : shift from original date
    CatchUp,    // ++ : shift from today until future
    Restart,    // .+ : shift from completion date
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgendaEntryType {
    Scheduled,
    Deadline,
    Closed,
}

pub struct AgendaView {
    focus_handle: FocusHandle,
    items: Vec<AgendaItem>,
    all_items: Vec<AgendaItem>, // Unfiltered items
    selected_index: usize,
    workspace: WeakEntity<Workspace>,
    show_done: bool,
    filter_tags: Vec<String>, // Changed from Option<String> to Vec<String> for multi-tag support
    filter_file: Option<String>,
    available_tags: Vec<String>,
    available_files: Vec<String>,
    days_range: i64, // Number of days to show (0 = all, 7 = week, 30 = month)
    shown_reminders: std::collections::HashSet<String>, // Track which reminders have been shown
    _reminder_timer: Option<gpui::Task<()>>, // Background timer for checking reminders
}

impl AgendaView {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();

        let mut view = Self {
            focus_handle,
            items: Vec::new(),
            all_items: Vec::new(),
            selected_index: 0,
            workspace,
            show_done: false,
            filter_tags: Vec::new(),
            filter_file: None,
            available_tags: Vec::new(),
            available_files: Vec::new(),
            days_range: 30, // Default to 30 days
            shown_reminders: std::collections::HashSet::new(),
            _reminder_timer: None,
        };

        // Start the background reminder checker
        view.start_reminder_timer(cx);
        view
    }

    fn start_reminder_timer(&mut self, _cx: &mut Context<Self>) {
        // For now, just check reminders on every collect_agenda_items call
        // A full background timer implementation would require more complex async handling
        // that's better suited for a future enhancement

        // Placeholder to satisfy the field requirement
        self._reminder_timer = None;
    }

    fn check_deadline_reminders(&mut self, cx: &mut Context<Self>) {
        let now = Local::now().naive_local();

        // Collect reminders to show (to avoid borrow checker issues)
        let mut reminders_to_show = Vec::new();

        // Check all items with REMIND durations
        for item in &self.all_items {
            // Only check SCHEDULED and DEADLINE items
            if item.entry_type != AgendaEntryType::Deadline
                && item.entry_type != AgendaEntryType::Scheduled
            {
                continue;
            }

            // Skip DONE tasks
            if let Some(todo) = &item.todo_keyword {
                if todo == "DONE" {
                    continue;
                }
            }

            // Skip items without REMIND duration
            let reminder_duration = match item.reminder_duration {
                Some(d) => d,
                None => {
                    log::info!("Skipping item without REMIND: {}", item.headline);
                    continue;
                }
            };

            log::info!(
                "Checking reminder for: {} (deadline: {}, remind: {:?})",
                item.headline,
                item.date,
                reminder_duration
            );

            // Create a unique ID for this reminder
            let reminder_id = format!("{}-{}", item.file_path.display(), item.line_number);

            // Skip if we've already shown this reminder
            if self.shown_reminders.contains(&reminder_id) {
                log::info!("Reminder already shown: {}", reminder_id);
                continue;
            }

            // Convert deadline date to datetime at end of day (23:59:59) to be more lenient
            let deadline_datetime = item.date.and_hms_opt(23, 59, 59).unwrap();

            // Calculate when the reminder should trigger
            let reminder_chrono_duration = chrono::Duration::from_std(reminder_duration).unwrap();
            let reminder_time = deadline_datetime - reminder_chrono_duration;

            log::info!(
                "Now: {}, Reminder time: {}, Deadline: {}",
                now,
                reminder_time,
                deadline_datetime
            );

            // Check if we should show the reminder now
            // Show if current time is past the reminder time but before the deadline
            if now >= reminder_time && now <= deadline_datetime {
                log::info!("TRIGGERING reminder for: {}", item.headline);

                // Calculate urgency message
                let days_until = (item.date - now.date()).num_days();
                let urgency = if days_until == 0 {
                    "TODAY".to_string()
                } else if days_until == 1 {
                    "TOMORROW".to_string()
                } else {
                    format!("in {} days", days_until)
                };

                reminders_to_show.push((
                    reminder_id,
                    item.headline.clone(),
                    item.date.format("%Y-%m-%d").to_string(),
                    urgency,
                    item.file_path.clone(),
                    item.line_number,
                ));
            } else {
                log::info!("Not yet time for reminder: {}", item.headline);
            }
        }

        log::info!("Total reminders to show: {}", reminders_to_show.len());

        // Now spawn notifications for collected reminders
        for (reminder_id, headline, date_str, urgency, file_path, line_number) in reminders_to_show
        {
            // Mark this reminder as shown
            self.shown_reminders.insert(reminder_id);

            // Spawn notification window
            self.spawn_reminder_notification(
                headline,
                date_str,
                urgency,
                file_path,
                line_number,
                cx,
            );
        }
    }

    fn spawn_reminder_notification(
        &mut self,
        task_name: String,
        deadline_date: String,
        urgency: String,
        file_path: PathBuf,
        line_number: u32,
        cx: &mut Context<Self>,
    ) {
        let workspace = match self.workspace.upgrade() {
            Some(ws) => ws,
            None => return,
        };

        // Get the displays to position the notification
        let displays = cx.displays();
        let display = displays.first().cloned();
        let Some(display) = display else { return };

        let options = DeadlineNotification::window_options(display, cx);

        // Open the notification window
        if let Ok(notification_window) = cx.open_window(options, |_, cx| {
            cx.new(|_| {
                DeadlineNotification::new(
                    task_name.clone(),
                    deadline_date.clone(),
                    urgency.clone(),
                    file_path.clone(),
                    line_number,
                    workspace.downgrade(),
                )
            })
        }) {
            // Get the notification entity from the window
            if let Ok(notification_entity) = notification_window.entity(cx) {
                // Clone the window handle for the subscription closure
                let window_handle = notification_window.clone();

                // Subscribe to notification events and close the window when dismissed/accepted
                cx.subscribe(
                    &notification_entity,
                    move |_this, _notification, event, cx| {
                        match event {
                            DeadlineNotificationEvent::Accepted => {
                                // TODO: Open the task in the editor
                                window_handle
                                    .update(cx, |_, window, _| {
                                        window.remove_window();
                                    })
                                    .ok();
                            }
                            DeadlineNotificationEvent::Dismissed => {
                                window_handle
                                    .update(cx, |_, window, _| {
                                        window.remove_window();
                                    })
                                    .ok();
                            }
                        }
                    },
                )
                .detach();
            }
        }

        // Also show a toast as backup
        let message = format!("⏰ Reminder {}: {} ({})", urgency, task_name, deadline_date);
        cx.defer(move |cx| {
            let _ = workspace.update(cx, |workspace, cx| {
                let toast = workspace::Toast::new(
                    workspace::notifications::NotificationId::unique::<DeadlineNotification>(),
                    message,
                );
                workspace.show_toast(toast, cx);
            });
        });
    }

    fn toggle_show_done(
        &mut self,
        _: &AgendaToggleDone,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_done = !self.show_done;
        self.collect_agenda_items(cx);
    }

    fn cycle_date_range(
        &mut self,
        _: &AgendaCycleDateRange,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.days_range = match self.days_range {
            7 => 30,
            30 => 90,
            90 => 0,
            _ => 7,
        };
        self.collect_agenda_items(cx);
    }

    fn cycle_tag_filter(
        &mut self,
        _: &AgendaCycleTagFilter,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.available_tags.is_empty() {
            return;
        }

        // Cycle to the next tag: if no tags selected, add first tag
        // If tags selected, find the last one and add the next, or clear if at end
        if self.filter_tags.is_empty() {
            self.filter_tags.push(self.available_tags[0].clone());
        } else {
            let last_tag = self.filter_tags.last().unwrap();
            if let Some(idx) = self.available_tags.iter().position(|t| t == last_tag) {
                if idx + 1 < self.available_tags.len() {
                    // Add next tag
                    let next_tag = self.available_tags[idx + 1].clone();
                    if !self.filter_tags.contains(&next_tag) {
                        self.filter_tags.push(next_tag);
                    }
                } else {
                    // Reached the end, clear all
                    self.filter_tags.clear();
                }
            } else {
                self.filter_tags.clear();
            }
        }

        self.apply_filters();
        cx.notify();
    }

    fn cycle_file_filter(
        &mut self,
        _: &AgendaCycleFileFilter,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.available_files.is_empty() {
            return;
        }

        self.filter_file = match &self.filter_file {
            None => Some(self.available_files[0].clone()),
            Some(current) => {
                if let Some(idx) = self.available_files.iter().position(|f| f == current) {
                    if idx + 1 < self.available_files.len() {
                        Some(self.available_files[idx + 1].clone())
                    } else {
                        None // Cycle back to "All"
                    }
                } else {
                    None
                }
            }
        };

        self.apply_filters();
        cx.notify();
    }

    fn clear_filters(
        &mut self,
        _: &AgendaClearFilters,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter_tags.clear();
        self.filter_file = None;
        self.show_done = false;
        self.days_range = 30;
        self.apply_filters();
        cx.notify();
    }

    fn pick_tag_filter(
        &mut self,
        _: &AgendaPickTagFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workspace) = self.workspace.upgrade() {
            let tags = self.available_tags.clone();
            let view_handle = cx.entity().downgrade();

            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    let delegate = AgendaTagPickerDelegate::new(tags, view_handle);
                    Picker::uniform_list(delegate, window, cx)
                        .width(rems(20.))
                        .max_height(Some(rems(20.).into()))
                });
            });
        }
    }

    fn pick_file_filter(
        &mut self,
        _: &AgendaPickFileFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workspace) = self.workspace.upgrade() {
            let files = self.available_files.clone();
            let view_handle = cx.entity().downgrade();

            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    let delegate = AgendaFilePickerDelegate::new(files, view_handle);
                    Picker::uniform_list(delegate, window, cx)
                        .width(rems(20.))
                        .max_height(Some(rems(20.).into()))
                });
            });
        }
    }

    fn pick_date_range(
        &mut self,
        _: &AgendaPickDateRange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workspace) = self.workspace.upgrade() {
            let view_handle = cx.entity().downgrade();

            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    let delegate = AgendaDateRangePickerDelegate::new(view_handle);
                    Picker::uniform_list(delegate, window, cx)
                        .width(rems(25.))
                        .max_height(Some(rems(20.).into()))
                });
            });
        }
    }

    fn apply_filters(&mut self) {
        let today = Local::now().naive_local().date();
        let mut filtered = self.all_items.clone();

        filtered.retain(|item| {
            // Filter by TODO state
            if !self.show_done {
                if let Some(todo) = &item.todo_keyword {
                    if todo == "DONE" {
                        return false;
                    }
                }
                if item.entry_type == AgendaEntryType::Closed {
                    return false;
                }
            }

            // Filter by date range
            if self.days_range > 0 {
                let days_diff = (item.date - today).num_days().abs();
                if days_diff > self.days_range {
                    return false;
                }
            }

            // Filter by tags (item must have at least one of the selected tags)
            if !self.filter_tags.is_empty() {
                if !self.filter_tags.iter().any(|tag| item.tags.contains(tag)) {
                    return false;
                }
            }

            // Filter by file
            if let Some(ref filter_file) = self.filter_file {
                let file_name = item
                    .file_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if file_name != filter_file {
                    return false;
                }
            }

            true
        });

        filtered.sort_by(|a, b| a.date.cmp(&b.date));
        self.items = filtered;
        self.selected_index = 0;
    }

    fn collect_agenda_items(&mut self, cx: &mut Context<Self>) {
        let settings = JournalSettings::get_global(cx);
        let Some(journal_dir) = journal_dir(&settings.path) else {
            log::warn!("Could not determine journal directory for agenda");
            return;
        };

        let mut items = Vec::new();

        fn scan_org_files(dir: &std::path::Path, items: &mut Vec<AgendaItem>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        scan_org_files(&path, items);
                    } else if path.extension().and_then(|s| s.to_str()) == Some("org") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            extract_agenda_items(&path, &content, items);
                        }
                    }
                }
            }
        }

        scan_org_files(&journal_dir, &mut items);

        // Extract unique tags and files for filtering
        use std::collections::HashSet;
        let mut tags_set: HashSet<String> = HashSet::new();
        let mut files_set: HashSet<String> = HashSet::new();

        for item in &items {
            for tag in &item.tags {
                tags_set.insert(tag.clone());
            }
            if let Some(file_name) = item.file_path.file_stem().and_then(|s| s.to_str()) {
                files_set.insert(file_name.to_string());
            }
        }

        self.available_tags = tags_set.into_iter().collect();
        self.available_tags.sort();

        self.available_files = files_set.into_iter().collect();
        self.available_files.sort();

        // Store all items before filtering
        items.sort_by(|a, b| a.date.cmp(&b.date));
        self.all_items = items;

        // Apply current filters
        self.apply_filters();

        // Check for deadline reminders
        self.check_deadline_reminders(cx);

        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_index + 1 < self.items.len() {
            self.selected_index += 1;
            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.items.get(self.selected_index) {
            let item = item.clone();
            if let Some(workspace) = self.workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    open_agenda_item(workspace, &item, window, cx);
                });
            }
        }
    }

    fn refresh(&mut self, _: &menu::Restart, _window: &mut Window, cx: &mut Context<Self>) {
        self.collect_agenda_items(cx);
    }
}

impl Render for AgendaView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let today = Local::now().naive_local().date();

        let range_label = match self.days_range {
            7 => "Week",
            30 => "Month",
            90 => "Quarter",
            _ => "All",
        };

        v_flex()
            .key_context("AgendaView")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::toggle_show_done))
            .on_action(cx.listener(Self::cycle_date_range))
            .on_action(cx.listener(Self::cycle_tag_filter))
            .on_action(cx.listener(Self::cycle_file_filter))
            .on_action(cx.listener(Self::clear_filters))
            .on_action(cx.listener(Self::pick_tag_filter))
            .on_action(cx.listener(Self::pick_file_filter))
            .on_action(cx.listener(Self::pick_date_range))
            .child(
                v_flex()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .p_4()
                            .pb_2()
                            .child(Label::new("Agenda").size(LabelSize::Large)),
                    )
                    .child(
                        h_flex()
                            .px_4()
                            .pb_3()
                            .gap_2()
                            .child(
                                Label::new(format!("Range: {}", range_label))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(if self.show_done {
                                    "• Showing Done"
                                } else {
                                    "• Hiding Done"
                                })
                                .size(LabelSize::XSmall)
                                .color(if self.show_done {
                                    Color::Success
                                } else {
                                    Color::Muted
                                }),
                            )
                            .when(!self.filter_tags.is_empty(), |this| {
                                this.children(self.filter_tags.iter().map(|tag| {
                                    Label::new(format!("• Tag: {}", tag))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent)
                                }))
                            })
                            .when_some(self.filter_file.as_ref(), |this, file| {
                                this.child(
                                    Label::new(format!("• File: {}", file))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                )
                            })
                            .child(
                                Label::new(format!("{} items", self.items.len()))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .children(self.render_items(today, cx))
    }
}

impl AgendaView {
    fn render_items(&self, today: chrono::NaiveDate, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        let mut current_date: Option<chrono::NaiveDate> = None;

        for (idx, item) in self.items.iter().enumerate() {
            // Add date header if date changed
            if current_date != Some(item.date) {
                current_date = Some(item.date);

                let date_label = if item.date == today {
                    "Today".to_string()
                } else if item.date == today + chrono::Duration::days(1) {
                    "Tomorrow".to_string()
                } else if item.date == today - chrono::Duration::days(1) {
                    "Yesterday".to_string()
                } else {
                    item.date.format("%A, %B %d, %Y").to_string()
                };

                let days_diff = (item.date - today).num_days();
                let date_color = if days_diff < 0 {
                    Color::Error // Past dates
                } else if days_diff == 0 {
                    Color::Accent // Today
                } else if days_diff <= 7 {
                    Color::Warning // Within a week
                } else {
                    Color::Default
                };

                elements.push(
                    h_flex()
                        .p_2()
                        .px_4()
                        .mt_2()
                        .child(
                            Label::new(date_label)
                                .color(date_color)
                                .size(LabelSize::Default),
                        )
                        .into_any_element(),
                );
            }

            // Render the item
            let selected = idx == self.selected_index;
            let item_bg = if selected {
                cx.theme().colors().element_selected
            } else {
                cx.theme().colors().element_background
            };

            let entry_icon = match item.entry_type {
                AgendaEntryType::Scheduled => "◷", // Clock icon
                AgendaEntryType::Deadline => "⚠",  // Warning icon
                AgendaEntryType::Closed => "✓",    // Check icon
            };

            let entry_color = match item.entry_type {
                AgendaEntryType::Scheduled => Color::Accent,
                AgendaEntryType::Deadline => Color::Warning,
                AgendaEntryType::Closed => Color::Success,
            };

            elements.push(
                h_flex()
                    .w_full()
                    .px_4()
                    .py_2()
                    .bg(item_bg)
                    .when(selected, |this| {
                        this.border_l_2()
                            .border_color(cx.theme().colors().border_focused)
                    })
                    .gap_3()
                    .child(
                        h_flex().flex_none().w_6().justify_center().child(
                            Label::new(entry_icon)
                                .color(entry_color)
                                .size(LabelSize::Default),
                        ),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .gap_2()
                            .items_center()
                            .when_some(item.todo_keyword.as_ref(), |this, keyword| {
                                let todo_color = match keyword.as_str() {
                                    "TODO" | "DOING" | "NEXT" | "STARTED" => Color::Warning,
                                    "DONE" => Color::Success,
                                    "WAITING" | "DEFERRED" => Color::Muted,
                                    "CANCELLED" | "CANCELED" => Color::Error,
                                    _ => Color::Default,
                                };
                                this.child(
                                    h_flex()
                                        .px_1()
                                        .rounded_sm()
                                        .bg(cx.theme().colors().element_hover)
                                        .child(
                                            Label::new(keyword.clone())
                                                .color(todo_color)
                                                .size(LabelSize::XSmall),
                                        ),
                                )
                            })
                            .child(Label::new(item.headline.clone()).size(LabelSize::Small))
                            .children(item.tags.iter().map(|tag| {
                                h_flex()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(cx.theme().colors().element_active)
                                    .child(
                                        Label::new(format!(":{tag}:"))
                                            .color(Color::Accent)
                                            .size(LabelSize::XSmall),
                                    )
                                    .into_any_element()
                            })),
                    )
                    .child(
                        h_flex().flex_none().child(
                            Label::new(
                                item.file_path
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("")
                                    .to_string(),
                            )
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                        ),
                    )
                    .into_any_element(),
            );
        }

        if elements.is_empty() {
            elements.push(
                v_flex()
                    .p_4()
                    .items_center()
                    .child(Label::new("No scheduled or deadline tasks found").color(Color::Muted))
                    .into_any_element(),
            );
        }

        elements
    }
}

impl Focusable for AgendaView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for AgendaView {}

impl workspace::Item for AgendaView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Agenda".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("agenda view")
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<Option<Entity<Self>>> {
        let view = cx.new(|cx| Self::new(self.workspace.clone(), cx));
        gpui::Task::ready(Some(view))
    }

    fn to_item_events(_event: &Self::Event, _f: impl FnMut(ItemEvent)) {}
}

fn extract_agenda_items(file_path: &std::path::Path, content: &str, items: &mut Vec<AgendaItem>) {
    use regex::Regex;

    // Regex to match SCHEDULED/DEADLINE/CLOSED with dates
    // DEADLINE can include warning period like: DEADLINE: <2024-12-25 Wed -5d>
    // REMIND syntax: REMIND: 10m, REMIND: 1h, REMIND: 2d, etc.
    // Repeater syntax: +1w, .+1d, ++1m (after the weekday in timestamp)
    let scheduled_re =
        Regex::new(r"SCHEDULED:\s*<(\d{4}-\d{2}-\d{2})\s+\w+(?:\s+([.+]{1,2})(\d+)([dwmy]))?")
            .unwrap();
    let deadline_re = Regex::new(
        r"DEADLINE:\s*<(\d{4}-\d{2}-\d{2})\s+\w+(?:\s+([.+]{1,2})(\d+)([dwmy]))?(?:\s+(-?\d+)d)?",
    )
    .unwrap();
    let closed_re = Regex::new(r"CLOSED:\s*\[(\d{4}-\d{2}-\d{2})").unwrap();
    let remind_re = Regex::new(r"REMIND:\s*(\d+)(m|h|d|w)").unwrap();
    let tag_re = Regex::new(r":([a-zA-Z0-9_@]+):").unwrap();
    let todo_re =
        Regex::new(r"^\*+\s+(TODO|DONE|DOING|WAITING|NEXT|STARTED|CANCELLED|CANCELED|DEFERRED)\s+")
            .unwrap();

    // Helper function to parse reminder duration from REMIND: syntax
    fn parse_reminder_duration(line: &str, remind_re: &Regex) -> Option<std::time::Duration> {
        remind_re.captures(line).and_then(|caps| {
            let value: u64 = caps[1].parse().ok()?;
            let unit = &caps[2];

            Some(match unit {
                "m" => std::time::Duration::from_secs(value * 60),
                "h" => std::time::Duration::from_secs(value * 3600),
                "d" => std::time::Duration::from_secs(value * 86400),
                "w" => std::time::Duration::from_secs(value * 604800),
                _ => return None,
            })
        })
    }

    // Helper function to parse repeater from captured regex groups
    fn parse_repeater(
        repeater_type_str: Option<&str>,
        interval_str: Option<&str>,
        unit_str: Option<&str>,
    ) -> Option<RepeaterInfo> {
        let repeater_type_str = repeater_type_str?;
        let interval_str = interval_str?;
        let unit_str = unit_str?;

        let interval: i64 = interval_str.parse().ok()?;

        let repeater_type = match repeater_type_str {
            "+" => RepeaterType::Cumulative,
            "++" => RepeaterType::CatchUp,
            ".+" => RepeaterType::Restart,
            _ => return None,
        };

        let unit = match unit_str {
            "d" => RepeaterUnit::Day,
            "w" => RepeaterUnit::Week,
            "m" => RepeaterUnit::Month,
            "y" => RepeaterUnit::Year,
            _ => return None,
        };

        Some(RepeaterInfo {
            interval,
            unit,
            repeater_type,
        })
    }

    let lines: Vec<&str> = content.lines().collect();
    let mut current_headline = String::new();
    let mut current_tags: Vec<String> = Vec::new();
    let mut current_todo: Option<String> = None;
    let mut current_reminder: Option<std::time::Duration> = None;

    for (line_num, line) in lines.iter().enumerate() {
        // Check if this is a headline
        if line.trim_start().starts_with('*') {
            // Extract tags from headline (at the end)
            current_tags.clear();
            let mut headline_text = line.trim().to_string();

            // Find and extract tags from the end of the headline
            if let Some(tag_start) = headline_text.rfind(" :") {
                let potential_tags = &headline_text[tag_start..];
                if potential_tags
                    .chars()
                    .all(|c| c == ':' || c == ' ' || c.is_alphanumeric() || c == '_' || c == '@')
                {
                    for cap in tag_re.captures_iter(potential_tags) {
                        current_tags.push(cap[1].to_string());
                    }
                    headline_text = headline_text[..tag_start].trim_end().to_string();
                }
            }

            // Extract TODO keyword and remove it from headline
            current_todo = None;
            if let Some(caps) = todo_re.captures(&headline_text) {
                current_todo = Some(caps[1].to_string());
                headline_text = todo_re.replace(&headline_text, "").to_string();
            }

            // Remove leading stars and whitespace
            headline_text = headline_text.trim_start_matches('*').trim().to_string();

            current_headline = headline_text;

            // Reset reminder for new headline
            current_reminder = None;
        }

        // Check for REMIND syntax (can appear on any line under a headline)
        if let Some(duration) = parse_reminder_duration(line, &remind_re) {
            current_reminder = Some(duration);
        }

        // Check for SCHEDULED
        if let Some(caps) = scheduled_re.captures(line) {
            if let Ok(date) = chrono::NaiveDate::parse_from_str(&caps[1], "%Y-%m-%d") {
                // Parse repeater if present (groups 2, 3, 4)
                let repeater = parse_repeater(
                    caps.get(2).map(|m| m.as_str()),
                    caps.get(3).map(|m| m.as_str()),
                    caps.get(4).map(|m| m.as_str()),
                );

                // Look ahead for REMIND on the next few lines
                let mut reminder_for_this_item = current_reminder;
                for next_line_idx in (line_num + 1)..lines.len().min(line_num + 5) {
                    let next_line = lines[next_line_idx];
                    // Stop if we hit another headline
                    if next_line.trim_start().starts_with('*') {
                        break;
                    }
                    // Check for REMIND
                    if let Some(duration) = parse_reminder_duration(next_line, &remind_re) {
                        reminder_for_this_item = Some(duration);
                        break;
                    }
                }

                items.push(AgendaItem {
                    file_path: file_path.to_path_buf(),
                    headline: current_headline.clone(),
                    date,
                    entry_type: AgendaEntryType::Scheduled,
                    line_number: line_num as u32,
                    tags: current_tags.clone(),
                    todo_keyword: current_todo.clone(),
                    warning_days: None, // Scheduled items don't have warnings
                    reminder_duration: reminder_for_this_item,
                    repeater,
                });
            }
        }

        // Check for DEADLINE
        if let Some(caps) = deadline_re.captures(line) {
            if let Ok(date) = chrono::NaiveDate::parse_from_str(&caps[1], "%Y-%m-%d") {
                // Parse repeater if present (groups 2, 3, 4)
                let repeater = parse_repeater(
                    caps.get(2).map(|m| m.as_str()),
                    caps.get(3).map(|m| m.as_str()),
                    caps.get(4).map(|m| m.as_str()),
                );

                // Extract warning days if present (e.g., -5d means warn 5 days before)
                // Note: warning days is now in group 5 after the repeater groups
                let warning_days = caps.get(5).and_then(|m| m.as_str().parse::<i64>().ok());

                // Look ahead for REMIND on the next few lines
                let mut reminder_for_this_item = current_reminder;
                for next_line_idx in (line_num + 1)..lines.len().min(line_num + 5) {
                    let next_line = lines[next_line_idx];
                    // Stop if we hit another headline
                    if next_line.trim_start().starts_with('*') {
                        break;
                    }
                    // Check for REMIND
                    if let Some(duration) = parse_reminder_duration(next_line, &remind_re) {
                        reminder_for_this_item = Some(duration);
                        break;
                    }
                }

                items.push(AgendaItem {
                    file_path: file_path.to_path_buf(),
                    headline: current_headline.clone(),
                    date,
                    entry_type: AgendaEntryType::Deadline,
                    line_number: line_num as u32,
                    tags: current_tags.clone(),
                    todo_keyword: current_todo.clone(),
                    warning_days,
                    reminder_duration: reminder_for_this_item,
                    repeater,
                });
            }
        }

        // Check for CLOSED
        if let Some(caps) = closed_re.captures(line) {
            if let Ok(date) = chrono::NaiveDate::parse_from_str(&caps[1], "%Y-%m-%d") {
                items.push(AgendaItem {
                    file_path: file_path.to_path_buf(),
                    headline: current_headline.clone(),
                    date,
                    entry_type: AgendaEntryType::Closed,
                    line_number: line_num as u32,
                    tags: current_tags.clone(),
                    todo_keyword: current_todo.clone(),
                    warning_days: None, // Closed items don't have warnings
                    repeater: None,     // Closed items don't have repeaters
                    reminder_duration: current_reminder,
                });
            }
        }
    }
}

fn open_agenda_item(
    workspace: &mut Workspace,
    item: &AgendaItem,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let path = item.file_path.clone();
    let line_number = item.line_number;

    let task = workspace.open_paths(
        vec![path],
        workspace::OpenOptions::default(),
        None,
        window,
        cx,
    );

    let workspace_weak = cx.entity().downgrade();
    cx.spawn_in(window, async move |_, cx| {
        // Wait for the file to open
        let items = task.await;

        // Get the first item (our opened file)
        if let Some(Some(Ok(item_handle))) = items.into_iter().next() {
            // Try to downcast to Editor
            if let Ok(editor) = item_handle.to_any().downcast::<Editor>() {
                // Navigate to the line using workspace context
                workspace_weak
                    .update_in(cx, |_, window, cx| {
                        // Convert line number (1-indexed) to Point (0-indexed row)
                        let row = line_number.saturating_sub(1);
                        let point = language::Point::new(row, 0);

                        editor.update(cx, |editor, cx| {
                            // Get the buffer and convert point to anchor
                            let buffer = editor.buffer().read(cx);
                            let snapshot = buffer.snapshot(cx);

                            // anchor_at returns an Anchor directly, not an Option
                            let anchor = snapshot.anchor_at(point, language::Bias::Left);

                            // Change selection and scroll to center
                            editor.change_selections(
                                editor::SelectionEffects::scroll(
                                    editor::scroll::Autoscroll::center(),
                                ),
                                window,
                                cx,
                                |s| s.select_ranges(Some(anchor..anchor)),
                            );
                        });
                    })
                    .ok();
            }
        }
    })
    .detach();
}

// Agenda tag filter picker
pub struct AgendaTagPickerDelegate {
    tags: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    agenda_view: WeakEntity<AgendaView>,
}

impl AgendaTagPickerDelegate {
    fn new(tags: Vec<String>, agenda_view: WeakEntity<AgendaView>) -> Self {
        let mut all_tags = vec!["All (clear filter)".to_string()];
        all_tags.extend(tags);

        Self {
            tags: all_tags.clone(),
            matches: all_tags
                .iter()
                .enumerate()
                .map(|(index, tag)| StringMatch {
                    candidate_id: index,
                    string: tag.clone(),
                    positions: Vec::new(),
                    score: 0.0,
                })
                .collect(),
            selected_index: 0,
            agenda_view,
        }
    }
}

impl PickerDelegate for AgendaTagPickerDelegate {
    type ListItem = ui::ListItem;

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select tag to filter by...".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let candidates = self
            .tags
            .iter()
            .enumerate()
            .map(|(id, tag)| StringMatchCandidate {
                id,
                string: tag.clone(),
                char_bag: tag.chars().collect(),
            })
            .collect::<Vec<_>>();

        let background_executor = cx.background_executor().clone();
        cx.spawn_in(_window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    false,
                    usize::MAX,
                    &Default::default(),
                    background_executor,
                )
                .await
            };

            this.update(cx, |this, cx| {
                let delegate = &mut this.delegate;
                delegate.matches = matches;
                delegate.selected_index = delegate
                    .selected_index
                    .min(delegate.matches.len().saturating_sub(1));
                cx.notify();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(mat) = self.matches.get(self.selected_index) {
            let tag = &self.tags[mat.candidate_id];

            if let Some(agenda_view) = self.agenda_view.upgrade() {
                agenda_view.update(cx, |view, cx| {
                    if tag == "All (clear filter)" {
                        view.filter_tags.clear();
                    } else {
                        // Toggle tag: add if not present, remove if present
                        if let Some(pos) = view.filter_tags.iter().position(|t| t == tag) {
                            view.filter_tags.remove(pos);
                        } else {
                            view.filter_tags.push(tag.clone());
                        }
                    }
                    view.apply_filters();
                    cx.notify();
                });
            }
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;

        Some(
            ui::ListItem::new(ix)
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(ui::HighlightedLabel::new(
                    mat.string.clone(),
                    mat.positions.clone(),
                )),
        )
    }
}

// Agenda file filter picker
pub struct AgendaFilePickerDelegate {
    files: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    agenda_view: WeakEntity<AgendaView>,
}

impl AgendaFilePickerDelegate {
    fn new(files: Vec<String>, agenda_view: WeakEntity<AgendaView>) -> Self {
        let mut all_files = vec!["All (clear filter)".to_string()];
        all_files.extend(files);

        Self {
            files: all_files.clone(),
            matches: all_files
                .iter()
                .enumerate()
                .map(|(index, file)| StringMatch {
                    candidate_id: index,
                    string: file.clone(),
                    positions: Vec::new(),
                    score: 0.0,
                })
                .collect(),
            selected_index: 0,
            agenda_view,
        }
    }
}

impl PickerDelegate for AgendaFilePickerDelegate {
    type ListItem = ui::ListItem;

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select file to filter by...".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let candidates = self
            .files
            .iter()
            .enumerate()
            .map(|(id, file)| StringMatchCandidate {
                id,
                string: file.clone(),
                char_bag: file.chars().collect(),
            })
            .collect::<Vec<_>>();

        let background_executor = cx.background_executor().clone();
        cx.spawn_in(_window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    false,
                    usize::MAX,
                    &Default::default(),
                    background_executor,
                )
                .await
            };

            this.update(cx, |this, cx| {
                let delegate = &mut this.delegate;
                delegate.matches = matches;
                delegate.selected_index = delegate
                    .selected_index
                    .min(delegate.matches.len().saturating_sub(1));
                cx.notify();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(mat) = self.matches.get(self.selected_index) {
            let file = &self.files[mat.candidate_id];

            if let Some(agenda_view) = self.agenda_view.upgrade() {
                agenda_view.update(cx, |view, cx| {
                    if file == "All (clear filter)" {
                        view.filter_file = None;
                    } else {
                        view.filter_file = Some(file.clone());
                    }
                    view.apply_filters();
                    cx.notify();
                });
            }
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;

        Some(
            ui::ListItem::new(ix)
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(ui::HighlightedLabel::new(
                    mat.string.clone(),
                    mat.positions.clone(),
                )),
        )
    }
}

// Agenda date range filter picker
pub struct AgendaDateRangePickerDelegate {
    ranges: Vec<(String, i64)>, // (label, days)
    matches: Vec<StringMatch>,
    selected_index: usize,
    agenda_view: WeakEntity<AgendaView>,
}

impl AgendaDateRangePickerDelegate {
    fn new(agenda_view: WeakEntity<AgendaView>) -> Self {
        let ranges = vec![
            ("All time".to_string(), 0),
            ("Today".to_string(), 0),
            ("Next 3 days".to_string(), 3),
            ("This week (7 days)".to_string(), 7),
            ("Next 2 weeks (14 days)".to_string(), 14),
            ("This month (30 days)".to_string(), 30),
            ("Next 2 months (60 days)".to_string(), 60),
            ("This quarter (90 days)".to_string(), 90),
            ("This year (365 days)".to_string(), 365),
        ];

        Self {
            ranges: ranges.clone(),
            matches: ranges
                .iter()
                .enumerate()
                .map(|(index, (label, _))| StringMatch {
                    candidate_id: index,
                    string: label.clone(),
                    positions: Vec::new(),
                    score: 0.0,
                })
                .collect(),
            selected_index: 0,
            agenda_view,
        }
    }
}

impl PickerDelegate for AgendaDateRangePickerDelegate {
    type ListItem = ui::ListItem;

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select date range...".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let candidates = self
            .ranges
            .iter()
            .enumerate()
            .map(|(id, (label, _))| StringMatchCandidate {
                id,
                string: label.clone(),
                char_bag: label.chars().collect(),
            })
            .collect::<Vec<_>>();

        let background_executor = cx.background_executor().clone();
        cx.spawn_in(_window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    false,
                    usize::MAX,
                    &Default::default(),
                    background_executor,
                )
                .await
            };

            this.update(cx, |this, cx| {
                let delegate = &mut this.delegate;
                delegate.matches = matches;
                delegate.selected_index = delegate
                    .selected_index
                    .min(delegate.matches.len().saturating_sub(1));
                cx.notify();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(mat) = self.matches.get(self.selected_index) {
            let (_, days) = &self.ranges[mat.candidate_id];

            if let Some(agenda_view) = self.agenda_view.upgrade() {
                agenda_view.update(cx, |view, cx| {
                    view.days_range = *days;
                    view.apply_filters();
                    cx.notify();
                });
            }
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;

        Some(
            ui::ListItem::new(ix)
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(ui::HighlightedLabel::new(
                    mat.string.clone(),
                    mat.positions.clone(),
                )),
        )
    }
}

// Tag completion provider for org-mode files
pub struct TagCompletionProvider {}

impl TagCompletionProvider {
    pub fn new() -> Self {
        Self {}
    }

    fn collect_tags_from_journal_directory(cx: &mut Context<Editor>) -> Vec<String> {
        let settings = JournalSettings::get_global(cx);
        let Some(journal_dir) = journal_dir(&settings.path) else {
            log::info!("Could not determine journal directory for tag collection");
            return Vec::new();
        };

        let mut all_tags = std::collections::HashSet::new();

        // Recursively scan all .org files in journal directory
        fn scan_directory(dir: &std::path::Path, tags: &mut std::collections::HashSet<String>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        scan_directory(&path, tags);
                    } else if path.extension().and_then(|s| s.to_str()) == Some("org") {
                        // Parse the file and extract tags
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            // Simple regex-based extraction for now (could use tree-sitter but this is faster)
                            // Match tags in headlines: :tag1:tag2:
                            let tag_regex = regex::Regex::new(r":([a-zA-Z0-9_@]+):").unwrap();
                            for cap in tag_regex.captures_iter(&content) {
                                if let Some(tag_match) = cap.get(1) {
                                    let tag = tag_match.as_str();
                                    tags.insert(tag.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        log::info!("Scanning journal directory for tags: {:?}", journal_dir);
        scan_directory(&journal_dir, &mut all_tags);
        log::info!(
            "Found {} unique tags across all journal files",
            all_tags.len()
        );

        all_tags.into_iter().collect()
    }

    fn collect_inherited_tags(buffer: &language::BufferSnapshot, current_row: u32) -> Vec<String> {
        // Get the syntax layer
        let Some(layer) = buffer.syntax_layers().next() else {
            return Vec::new();
        };

        let root_node = layer.node();

        // Find all headlines and their tags, tracking hierarchy
        let mut headlines_with_tags: Vec<(u32, u32, Vec<String>)> = Vec::new(); // (row, level, tags)

        fn collect_headlines<'a>(
            node: tree_sitter::Node<'a>,
            buffer: &language::BufferSnapshot,
            headlines: &mut Vec<(u32, u32, Vec<String>)>,
        ) {
            if node.kind() == "headline" {
                let row = node.start_position().row as u32;

                // Count stars to determine level
                let mut star_count = 0;
                let mut tags = Vec::new();

                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "stars" {
                        let stars_text = buffer
                            .text_for_range(child.byte_range())
                            .collect::<String>();
                        star_count = stars_text.chars().filter(|c| *c == '*').count() as u32;
                    } else if child.kind() == "tag_list" {
                        // Extract all tags from this tag_list
                        let mut tag_cursor = child.walk();
                        for tag_child in child.children(&mut tag_cursor) {
                            if tag_child.kind() == "tag" {
                                let tag_text = buffer
                                    .text_for_range(tag_child.byte_range())
                                    .collect::<String>();
                                let clean_tag = tag_text.trim_matches(':');
                                if !clean_tag.is_empty() {
                                    tags.push(clean_tag.to_string());
                                }
                            }
                        }
                    }
                }

                headlines.push((row, star_count, tags));
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_headlines(child, buffer, headlines);
            }
        }

        collect_headlines(root_node, buffer, &mut headlines_with_tags);

        // Find the current headline and collect inherited tags
        let mut inherited_tags = Vec::new();
        let mut current_level = None;

        // First, find the current headline's level
        for (row, level, _tags) in &headlines_with_tags {
            if *row == current_row {
                current_level = Some(*level);
                break;
            }
        }

        if let Some(curr_level) = current_level {
            // Walk backwards through headlines to find ancestors
            for (row, level, tags) in headlines_with_tags.iter().rev() {
                if *row < current_row && *level < curr_level {
                    // This is an ancestor headline, inherit its tags
                    for tag in tags {
                        if !inherited_tags.contains(tag) {
                            inherited_tags.push(tag.clone());
                        }
                    }
                }
            }
        }

        inherited_tags
    }

    fn collect_tags_from_buffer(buffer: &language::BufferSnapshot) -> Vec<String> {
        let mut tags = std::collections::HashSet::new();

        // Get the syntax layers - org-mode files have language at layer 0
        let Some(layer) = buffer.syntax_layers().next() else {
            log::info!("No syntax layers found in buffer");
            return Vec::new();
        };

        log::info!("Found syntax layer");

        let root_node = layer.node();

        // Simpler approach: recursively visit all nodes
        fn visit_node<'a>(
            node: tree_sitter::Node<'a>,
            buffer: &language::BufferSnapshot,
            tags: &mut std::collections::HashSet<String>,
            depth: usize,
        ) {
            // Check if this is a tag node
            if node.kind() == "tag" {
                let tag_text = buffer.text_for_range(node.byte_range()).collect::<String>();
                let start = node.start_position();
                let end = node.end_position();
                log::info!(
                    "{}[TAG] at line {}, col {}-{}: raw={:?}",
                    "  ".repeat(depth),
                    start.row + 1,
                    start.column,
                    end.column,
                    tag_text
                );
                // Tag text includes the colons like ":tag:", so strip them
                let clean_tag = tag_text.trim_matches(':');
                if !clean_tag.is_empty() {
                    tags.insert(clean_tag.to_string());
                    log::info!("{}  -> cleaned: {}", "  ".repeat(depth), clean_tag);
                }
            }

            // Log headline nodes to see context
            if node.kind() == "headline" {
                let headline_text = buffer.text_for_range(node.byte_range()).collect::<String>();
                let start = node.start_position();
                log::info!(
                    "{}[HEADLINE] at line {}: {:?}",
                    "  ".repeat(depth),
                    start.row + 1,
                    headline_text
                        .trim_end()
                        .chars()
                        .take(60)
                        .collect::<String>()
                );
            }

            // Recursively visit children
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, buffer, tags, depth + 1);
            }
        }

        visit_node(root_node, buffer, &mut tags, 0);

        log::info!("Total tags collected: {}", tags.len());
        let result: Vec<String> = tags.into_iter().collect();
        log::info!("Tags: {:?}", result);
        result
    }
}

impl editor::CompletionProvider for TagCompletionProvider {
    fn completions(
        &self,
        _excerpt_id: editor::ExcerptId,
        buffer: &Entity<language::Buffer>,
        buffer_position: language::Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> gpui::Task<anyhow::Result<Vec<CompletionResponse>>> {
        log::info!("TagCompletionProvider::completions called");

        let buffer_snapshot = buffer.read(cx).snapshot();
        let position = buffer_position.to_point(&buffer_snapshot);
        let line_start = language::Point::new(position.row, 0);
        let line_end = language::Point::new(position.row, position.column);

        let line_text = buffer_snapshot
            .text_for_range(line_start..line_end)
            .collect::<String>();

        log::info!("Completions line_text: {:?}", line_text);

        // Check if we're on a headline and after a colon
        let trimmed = line_text.trim_start();
        if !trimmed.starts_with('*') {
            log::info!("Not on a headline, returning empty");
            return gpui::Task::ready(Ok(Vec::new()));
        }

        // Find the last colon before cursor
        let Some(last_colon_pos) = line_text.rfind(':') else {
            log::info!("No colon found, returning empty");
            return gpui::Task::ready(Ok(Vec::new()));
        };

        log::info!("Found colon at position {}", last_colon_pos);

        // Extract the partial tag being typed
        let partial_tag = &line_text[last_colon_pos + 1..];

        // Get tags from current buffer
        let mut all_tags = Self::collect_tags_from_buffer(&buffer_snapshot);

        // Collect inherited tags from parent headlines
        let inherited_tags = Self::collect_inherited_tags(&buffer_snapshot, position.row);

        // Collect tags from all journal files
        let journal_tags = Self::collect_tags_from_journal_directory(cx);

        // Merge all tag sources
        for tag in &inherited_tags {
            if !all_tags.contains(tag) {
                all_tags.push(tag.clone());
            }
        }

        for tag in &journal_tags {
            if !all_tags.contains(tag) {
                all_tags.push(tag.clone());
            }
        }

        log::info!(
            "Collected {} tags total (buffer: {}, inherited: {}, journal: {})",
            all_tags.len(),
            Self::collect_tags_from_buffer(&buffer_snapshot).len(),
            inherited_tags.len(),
            journal_tags.len()
        );

        // Filter tags based on partial input
        let partial_tag_lower = partial_tag.to_lowercase();
        all_tags.retain(|tag| tag.to_lowercase().contains(&partial_tag_lower));
        all_tags.sort();
        all_tags.dedup();

        log::info!(
            "After filtering by '{}': {} tags: {:?}",
            partial_tag,
            all_tags.len(),
            all_tags
        );

        // Convert to range for replacement
        let line_offset = buffer_snapshot.point_to_offset(line_start);
        let start_offset = line_offset + last_colon_pos + 1;
        let end_offset = line_offset + line_text.len();

        let replace_start = buffer_snapshot.anchor_before(start_offset);
        let replace_end = buffer_snapshot.anchor_after(end_offset);

        // Create completions
        let completions = all_tags
            .into_iter()
            .map(|tag| Completion {
                replace_range: replace_start..replace_end,
                new_text: format!("{}:", tag),
                label: language::CodeLabel::plain(tag, None),
                documentation: None,
                source: CompletionSource::Custom,
                icon_path: Some(ui::IconName::Hash.path().into()),
                match_start: None,
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
            })
            .collect();

        gpui::Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions::default(),
            is_incomplete: false,
        }]))
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<language::Buffer>,
        position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        _menu_is_open: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        log::info!(
            "TagCompletionProvider::is_completion_trigger called with text: {:?}",
            text
        );

        // Trigger on ':' character in org-mode headlines
        if text != ":" {
            log::info!("Text is not ':', returning false");
            return false;
        }

        let buffer_snapshot = buffer.read(cx).snapshot();
        let position_point = position.to_point(&buffer_snapshot);
        let line_start = language::Point::new(position_point.row, 0);
        let line_text = buffer_snapshot
            .text_for_range(line_start..position_point)
            .collect::<String>();

        log::info!("Line text: {:?}", line_text);

        // Check if we're on a headline (starts with *)
        let trimmed = line_text.trim_start();
        let is_headline = trimmed.starts_with('*');

        log::info!("Is headline: {}", is_headline);
        is_headline
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    mod heading_entry_tests {
        use super::super::*;

        #[test]
        fn test_heading_entry_defaults_to_hour_12() {
            let naive_time = NaiveTime::from_hms_milli_opt(15, 0, 0, 0).unwrap();
            let actual_heading_entry = heading_entry(naive_time, &HourFormat::Hour12);
            let expected_heading_entry = "# 3:00 PM";

            assert_eq!(actual_heading_entry, expected_heading_entry);
        }

        #[test]
        fn test_heading_entry_is_hour_12() {
            let naive_time = NaiveTime::from_hms_milli_opt(15, 0, 0, 0).unwrap();
            let actual_heading_entry = heading_entry(naive_time, &HourFormat::Hour12);
            let expected_heading_entry = "# 3:00 PM";

            assert_eq!(actual_heading_entry, expected_heading_entry);
        }

        #[test]
        fn test_heading_entry_is_hour_24() {
            let naive_time = NaiveTime::from_hms_milli_opt(15, 0, 0, 0).unwrap();
            let actual_heading_entry = heading_entry(naive_time, &HourFormat::Hour24);
            let expected_heading_entry = "# 15:00";

            assert_eq!(actual_heading_entry, expected_heading_entry);
        }
    }

    mod journal_dir_tests {
        use super::super::*;

        #[test]
        #[cfg(target_family = "unix")]
        fn test_absolute_unix_path() {
            let result = journal_dir("/home/user");
            assert!(result.is_some());
            let path = result.unwrap();
            assert!(path.is_absolute());
            assert_eq!(path, PathBuf::from("/home/user/journal"));
        }

        #[test]
        fn test_tilde_expansion() {
            let result = journal_dir("~/documents");
            assert!(result.is_some());
            let path = result.unwrap();

            assert!(path.is_absolute(), "Tilde should expand to absolute path");

            if let Some(home) = std::env::home_dir() {
                assert_eq!(path, home.join("documents").join("journal"));
            }
        }

        #[test]
        fn test_relative_path_falls_back_to_home() {
            for relative_path in ["relative/path", "NONEXT/some/path", "../some/path"] {
                let result = journal_dir(relative_path);
                assert!(result.is_some(), "Failed for path: {}", relative_path);
                let path = result.unwrap();

                assert!(
                    path.is_absolute(),
                    "Path should be absolute for input '{}', got: {:?}",
                    relative_path,
                    path
                );

                if let Some(home) = std::env::home_dir() {
                    assert_eq!(
                        path,
                        home.join("journal"),
                        "Should fall back to home directory for input '{}'",
                        relative_path
                    );
                }
            }
        }

        #[test]
        #[cfg(target_os = "windows")]
        fn test_absolute_path_windows_style() {
            let result = journal_dir("C:\\Users\\user\\Documents");
            assert!(result.is_some());
            let path = result.unwrap();
            assert_eq!(path, PathBuf::from("C:\\Users\\user\\Documents\\journal"));
        }
    }
}
