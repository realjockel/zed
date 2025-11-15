use chrono::{Datelike, Local, NaiveTime, Timelike};
use editor::scroll::Autoscroll;
use editor::{self, Editor, SelectionEffects, ToOffset, ToPoint};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Global, IntoElement, ParentElement, Render, Styled, Subscription, WeakEntity,
    Window, actions,
};
use language::Point;
use menu;
use multi_buffer::{ExcerptRange, MultiBufferSnapshot};
use picker::{Picker, PickerDelegate};
pub use settings::HourFormat;
use settings::{ActiveSettingsProfileName, CaptureTemplateConfig, RegisterSetting, Settings};
use std::{
    fs::OpenOptions,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use theme::ActiveTheme;
use ui::{HighlightedLabel, Label, LabelSize, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use util::paths::PathStyle;
use util::rel_path::RelPathBuf;
use workspace::{AppState, ModalView, OpenVisible, Workspace};

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
}

impl settings::Settings for JournalSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let journal = content.journal.clone().unwrap();

        Self {
            path: journal.path.unwrap(),
            hour_format: journal.hour_format.unwrap(),
            capture_templates: journal.capture_templates.unwrap_or_default(),
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
    let month_dir = journal_dir
        .join(format!("{:02}", now.year()))
        .join(format!("{:02}", now.month()));
    let entry_path = month_dir.join(format!("{:02}.md", now.day()));
    let now = now.time();
    let entry_heading = heading_entry(now, &settings.hour_format);

    let create_entry = cx.background_spawn(async move {
        std::fs::create_dir_all(month_dir)?;
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&entry_path)?;
        Ok::<_, std::io::Error>((journal_dir, entry_path))
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
            let (journal_dir, entry_path) = create_entry.await?;
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
                    editor.insert(&entry_heading, window, cx);
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

fn heading_entry(now: NaiveTime, hour_format: &HourFormat) -> String {
    match hour_format {
        HourFormat::Hour24 => {
            let hour = now.hour();
            format!("# {}:{:02}", hour, now.minute())
        }
        HourFormat::Hour12 => {
            let (pm, hour) = now.hour12();
            let am_or_pm = if pm { "PM" } else { "AM" };
            format!("# {}:{:02} {}", hour, now.minute(), am_or_pm)
        }
    }
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
                } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    // Scan markdown file for tags
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
                } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
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
                    let now = chrono::Local::now();
                    let target_path = journal_dir
                        .join(format!("{:04}", now.year()))
                        .join(format!("{:02}", now.month()))
                        .join(format!("{:02}.md", now.day()));
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
                let now = chrono::Local::now();
                let target_path = journal_dir
                    .join(format!("{:04}", now.year()))
                    .join(format!("{:02}", now.month()))
                    .join(format!("{:02}.md", now.day()));
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
                                if path.extension().and_then(|s| s.to_str()) == Some("md") {
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
            let day = components[0].as_os_str().to_str()?.trim_end_matches(".md");
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
                .join(format!("{}.md", parts[2]));
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
