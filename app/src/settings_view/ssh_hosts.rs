//! Settings → SSH hosts page.
//!
//! Surfaces the host aliases parsed by [`crate::ssh_hosts::SshHostsModel`]
//! from `~/.ssh/config` as a Settings page (Phase 1.3 of
//! [discussion #442](https://github.com/warpdotdev/warp/discussions/442)).
//! Each entry shows the alias plus the resolved connect-time metadata
//! ssh would use (hostname, user, port, identity files, ProxyJump).
//!
//! - Phase 1.4 (click-to-connect): a click on the main row surface
//!   opens a fresh terminal tab and submits `ssh <alias>` in one go.
//! - Phase 1.5a (metadata display): when [`crate::ssh_hosts_metadata::SshHostsMetadataModel`]
//!   has a record for an alias, the row shows the display-name
//!   override, color swatch, tag chips, and notes.
//! - Phase 1.5b (metadata editor): each row carries an "Edit" button;
//!   clicking it opens [`super::ssh_hosts_metadata_edit_dialog::SshHostsMetadataEditDialog`],
//!   which writes back through the model on Save.
//! - Phase 1.6 (local-only cross-device sync): "Export…" / "Import…"
//!   buttons at the top of the page write/read the full metadata
//!   store as a portable TOML file. Users sync via their own tool of
//!   choice (Dropbox, iCloud Drive, syncthing, git, scp). Server-side
//!   Warp Drive sync is out of scope here because adding a new
//!   `JsonObjectType` requires a coordinated server schema change.
//!
//! The page live-refreshes via the model's `HostsUpdated` and
//! `Updated` events whenever the underlying state changes on disk.

use std::path::PathBuf;

use warp_core::ui::color::hex_color::coloru_from_hex_string;
use warp_ssh_config::HostDetail;
use warpui::elements::{
    Border, ChildView, ConstrainedBox, Container, CornerRadius, CrossAxisAlignment, Element, Flex,
    Hoverable, MainAxisAlignment, MainAxisSize, MouseStateHandle, Padding, ParentElement, Radius,
    Wrap,
};
use warpui::platform::{Cursor, FilePickerConfiguration, SaveFilePickerConfiguration};
use warpui::ui_components::components::UiComponent;
use warpui::{AppContext, Entity, SingletonEntity, TypedActionView, View, ViewContext, ViewHandle};

use super::settings_page::{
    MatchData, PageType, SettingsPageEvent, SettingsPageMeta, SettingsPageViewHandle,
    SettingsWidget,
};
use super::ssh_hosts_metadata_edit_dialog::{
    SshHostsMetadataEditDialog, SshHostsMetadataEditDialogEvent,
};
use super::SettingsSection;
use crate::appearance::Appearance;
use crate::ssh_hosts::{SshHostsEvent, SshHostsModel};
use crate::ssh_hosts_metadata::{
    HostMetadata, ImportResult, SshHostsMetadataEvent, SshHostsMetadataModel,
};
use crate::view_components::DismissibleToast;
use crate::workspace::WorkspaceAction;
use crate::ToastStack;
use warp_ssh_diagnostics::{list_ssh_identities, ssh_agent_status, SshAgentStatus, SshIdentity};

const ROW_SPACING: f32 = 12.;
const ROW_INTERIOR_SPACING: f32 = 2.;
const DESCRIPTION_BOTTOM_MARGIN: f32 = 12.;
const ROW_PADDING: f32 = 8.;
const COLOR_SWATCH_SIZE: f32 = 10.;
const CHIP_SPACING: f32 = 4.;
const CHIP_HORIZONTAL_PADDING: f32 = 6.;
const CHIP_VERTICAL_PADDING: f32 = 2.;
const CHIP_ROW_TOP_MARGIN: f32 = 6.;
const EDIT_BUTTON_PADDING: f32 = 6.;
const EXPORT_DEFAULT_FILENAME: &str = "ssh_hosts_metadata.toml";
const ACTIONS_ROW_BOTTOM_MARGIN: f32 = 12.;
const ACTIONS_ROW_SPACING: f32 = 8.;

/// Page-level actions routed via [`TypedActionView`]. Edit buttons in
/// each host row dispatch [`SshHostsSettingsPageAction::OpenMetadataEditor`]
/// rather than mutating page state directly, because per-row click
/// closures only have access to an `EventContext` (no direct view
/// state access).
#[derive(Debug, Clone)]
pub enum SshHostsSettingsPageAction {
    /// Open the metadata edit dialog for the given alias, pre-filled
    /// with the current record (or empty fields when no record exists).
    OpenMetadataEditor(String),
    /// Phase 1.6 — open a native save dialog and write the full
    /// metadata store to the chosen path.
    ExportMetadata,
    /// Phase 1.6 — open a native file dialog and merge a previously
    /// exported metadata TOML into the current store.
    ImportMetadata,
    /// R3.5 — re-probe `ssh-agent` + re-scan `~/.ssh` for visible
    /// identity files. The `ssh-add -l` subprocess is too expensive
    /// to run on every render so the diagnostics row caches the
    /// result and the user (or an internal refresh trigger) drives
    /// re-probing via this action.
    RefreshDiagnostics,
}

pub struct SshHostsSettingsPageView {
    page: PageType<Self>,
    /// Per-row mouse-state handles, indexed in the same order as the
    /// host list snapshot held by [`SshHostsModel`]. Kept in sync with
    /// the host list via the model subscription.
    row_states: Vec<MouseStateHandle>,
    /// Per-row edit-button mouse states. Same indexing as `row_states`.
    edit_button_states: Vec<MouseStateHandle>,
    /// The metadata edit dialog, rendered into the modal slot when
    /// [`SshHostsMetadataEditDialog::is_visible`] is true.
    metadata_edit_dialog: ViewHandle<SshHostsMetadataEditDialog>,
    /// Mouse-state for the page-level "Export…" button (Phase 1.6).
    export_button_state: MouseStateHandle,
    /// Mouse-state for the page-level "Import…" button (Phase 1.6).
    import_button_state: MouseStateHandle,
    /// Mouse-state for the diagnostics-refresh button (R3.5).
    refresh_diagnostics_button_state: MouseStateHandle,
    /// Cached `ssh-agent` probe result. Refreshed on demand via the
    /// chip's button rather than on every render — `ssh-add -l`
    /// spawns a subprocess (~50ms typical, ~750ms worst case) which
    /// would stall the page's settings-tab search reflow.
    agent_status: SshAgentStatus,
    /// Cached identity-file list. Cheap (filesystem stat) but
    /// cached alongside `agent_status` for symmetric refresh
    /// semantics.
    identities: Vec<SshIdentity>,
}

impl SshHostsSettingsPageView {
    pub fn new(ctx: &mut ViewContext<SshHostsSettingsPageView>) -> Self {
        // Re-render when the parsed host list changes on disk, and
        // resize the per-row handle vectors to match.
        let model = SshHostsModel::handle(ctx);
        ctx.subscribe_to_model(&model, |me, _, event, ctx| match event {
            SshHostsEvent::HostsUpdated(new_hosts) => {
                me.row_states
                    .resize_with(new_hosts.len(), MouseStateHandle::default);
                me.edit_button_states
                    .resize_with(new_hosts.len(), MouseStateHandle::default);
                ctx.notify();
            }
        });

        // Re-render when host metadata changes (TOML edit, programmatic
        // set/remove). No payload to thread through — the widget re-reads
        // the singleton on each render.
        let metadata = SshHostsMetadataModel::handle(ctx);
        ctx.subscribe_to_model(&metadata, |_, _, event, ctx| match event {
            SshHostsMetadataEvent::Updated => {
                ctx.notify();
            }
        });

        let metadata_edit_dialog = ctx.add_typed_action_view(SshHostsMetadataEditDialog::new);
        ctx.subscribe_to_view(&metadata_edit_dialog, |me, _, event, ctx| {
            me.handle_metadata_edit_dialog_event(event, ctx);
        });

        let initial_count = SshHostsModel::as_ref(ctx).hosts().len();
        let row_states = (0..initial_count).map(|_| Default::default()).collect();
        let edit_button_states = (0..initial_count).map(|_| Default::default()).collect();

        Self {
            page: PageType::new_monolith(SshHostsWidget, Some("SSH hosts"), false),
            row_states,
            edit_button_states,
            metadata_edit_dialog,
            export_button_state: MouseStateHandle::default(),
            import_button_state: MouseStateHandle::default(),
            refresh_diagnostics_button_state: MouseStateHandle::default(),
            agent_status: ssh_agent_status(),
            identities: list_ssh_identities(),
        }
    }

    fn handle_metadata_edit_dialog_event(
        &mut self,
        event: &SshHostsMetadataEditDialogEvent,
        ctx: &mut ViewContext<Self>,
    ) {
        let dialog = self.metadata_edit_dialog.clone();
        match event {
            SshHostsMetadataEditDialogEvent::Save { alias, metadata } => {
                let alias = alias.clone();
                let metadata = metadata.clone();
                SshHostsMetadataModel::handle(ctx).update(ctx, move |model, ctx| {
                    model.set(alias, metadata, ctx);
                });
                dialog.update(ctx, |d, ctx| d.hide(ctx));
            }
            SshHostsMetadataEditDialogEvent::Cancel => {
                dialog.update(ctx, |d, ctx| d.hide(ctx));
            }
        }
    }

    /// Returns the dialog element when it's open. Wired through
    /// `settings_view::mod.rs::get_modal_content_for_page` so the
    /// Settings shell renders it as a top-level overlay.
    pub fn get_modal_content(&self, app: &AppContext) -> Option<Box<dyn Element>> {
        if self.metadata_edit_dialog.as_ref(app).is_visible() {
            Some(ChildView::new(&self.metadata_edit_dialog).finish())
        } else {
            None
        }
    }
}

impl TypedActionView for SshHostsSettingsPageView {
    type Action = SshHostsSettingsPageAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            SshHostsSettingsPageAction::OpenMetadataEditor(alias) => {
                let alias = alias.clone();
                let current = SshHostsMetadataModel::as_ref(ctx).get(&alias).cloned();
                let dialog = self.metadata_edit_dialog.clone();
                dialog.update(ctx, move |d, ctx| {
                    d.show(alias, current.as_ref(), ctx);
                });
            }
            SshHostsSettingsPageAction::ExportMetadata => {
                let mut config = SaveFilePickerConfiguration::new()
                    .with_default_filename(EXPORT_DEFAULT_FILENAME.to_string());
                if let Some(home) = dirs::home_dir() {
                    config = config.with_default_directory(home);
                }
                ctx.open_save_file_picker(
                    |path_opt, _view, ctx| {
                        let Some(path_str) = path_opt else { return };
                        let path = PathBuf::from(path_str);
                        let result = SshHostsMetadataModel::as_ref(ctx).export_to_path(&path);
                        emit_export_toast(&path, result, ctx);
                    },
                    config,
                );
            }
            SshHostsSettingsPageAction::ImportMetadata => {
                let config = FilePickerConfiguration::new();
                ctx.open_file_picker(
                    |result, ctx| {
                        let Ok(paths) = result else { return };
                        let Some(path_str) = paths.into_iter().next() else {
                            return;
                        };
                        let path = PathBuf::from(path_str);
                        let model_handle = SshHostsMetadataModel::handle(ctx);
                        let outcome = model_handle
                            .update(ctx, |model, ctx| model.import_from_path(&path, ctx));
                        emit_import_toast(&path, outcome, ctx);
                    },
                    config,
                );
            }
            SshHostsSettingsPageAction::RefreshDiagnostics => {
                self.agent_status = ssh_agent_status();
                self.identities = list_ssh_identities();
                ctx.notify();
            }
        }
    }
}

impl Entity for SshHostsSettingsPageView {
    type Event = SettingsPageEvent;
}

impl View for SshHostsSettingsPageView {
    fn ui_name() -> &'static str {
        "SshHostsPage"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        self.page.render(self, app)
    }
}

struct SshHostsWidget;

impl SettingsWidget for SshHostsWidget {
    type View = SshHostsSettingsPageView;

    fn search_terms(&self) -> &str {
        "ssh hosts remote connection config alias hostname proxy identity"
    }

    fn render(
        &self,
        view: &SshHostsSettingsPageView,
        appearance: &Appearance,
        app: &AppContext,
    ) -> Box<dyn Element> {
        let hosts = SshHostsModel::as_ref(app).hosts();
        let metadata = SshHostsMetadataModel::as_ref(app);
        let mut column = Flex::column()
            .with_main_axis_size(MainAxisSize::Min)
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch);

        column.add_child(render_description(appearance));
        column.add_child(render_diagnostics_row(
            &view.agent_status,
            &view.identities,
            view.refresh_diagnostics_button_state.clone(),
            appearance,
        ));
        column.add_child(render_actions_row(
            view.export_button_state.clone(),
            view.import_button_state.clone(),
            appearance,
        ));

        if hosts.is_empty() {
            column.add_child(render_empty_state(appearance));
        } else {
            for (idx, host) in hosts.iter().enumerate() {
                // Fall back to a fresh handle if `row_states` is briefly
                // out of sync with the host list (the resize fires on
                // the next `HostsUpdated` after a config edit).
                let mouse_state = view.row_states.get(idx).cloned().unwrap_or_default();
                let edit_state = view
                    .edit_button_states
                    .get(idx)
                    .cloned()
                    .unwrap_or_default();
                let row_metadata = metadata.get(&host.alias);
                column.add_child(render_host_row(
                    host,
                    row_metadata,
                    mouse_state,
                    edit_state,
                    appearance,
                ));
            }
        }

        column.finish()
    }
}

impl SettingsPageMeta for SshHostsSettingsPageView {
    fn section() -> SettingsSection {
        SettingsSection::SshHosts
    }

    fn should_render(&self, _ctx: &AppContext) -> bool {
        true
    }

    fn update_filter(&mut self, query: &str, ctx: &mut ViewContext<Self>) -> MatchData {
        self.page.update_filter(query, ctx)
    }

    fn scroll_to_widget(&mut self, widget_id: &'static str) {
        self.page.scroll_to_widget(widget_id)
    }

    fn clear_highlighted_widget(&mut self) {
        self.page.clear_highlighted_widget();
    }
}

impl From<ViewHandle<SshHostsSettingsPageView>> for SettingsPageViewHandle {
    fn from(view_handle: ViewHandle<SshHostsSettingsPageView>) -> Self {
        SettingsPageViewHandle::SshHosts(view_handle)
    }
}

fn render_description(appearance: &Appearance) -> Box<dyn Element> {
    Container::new(
        appearance
            .ui_builder()
            .span(
                "Hosts found in your ~/.ssh/config. Edit the file directly to add, remove, or \
                 rename hosts; this list refreshes automatically when the file changes on disk."
                    .to_string(),
            )
            .with_soft_wrap()
            .build()
            .finish(),
    )
    .with_margin_bottom(DESCRIPTION_BOTTOM_MARGIN)
    .finish()
}

fn render_empty_state(appearance: &Appearance) -> Box<dyn Element> {
    appearance
        .ui_builder()
        .span(
            "No SSH hosts found. Add a `Host` block to ~/.ssh/config to populate this list."
                .to_string(),
        )
        .with_soft_wrap()
        .build()
        .finish()
}

fn render_host_row(
    host: &HostDetail,
    metadata: Option<&HostMetadata>,
    mouse_state: MouseStateHandle,
    edit_state: MouseStateHandle,
    appearance: &Appearance,
) -> Box<dyn Element> {
    // Captured by the connect click handler.
    let alias_for_dispatch = host.alias.clone();
    // Captured by the edit click handler.
    let alias_for_edit = host.alias.clone();

    let connect_surface = Hoverable::new(mouse_state, |_state| {
        let ui = appearance.ui_builder();

        // Title row: optional color swatch + display name (falls back
        // to the alias when no `display_name` override is set).
        let label = metadata
            .and_then(|m| m.display_name.as_deref())
            .unwrap_or(host.alias.as_str())
            .to_string();
        let mut title_row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_spacing(CHIP_SPACING);
        if let Some(swatch) = metadata
            .and_then(|m| m.color.as_deref())
            .and_then(|hex| render_color_swatch(hex, appearance))
        {
            title_row.add_child(swatch);
        }
        title_row.add_child(ui.span(label.clone()).build().finish());

        let mut row = Flex::column()
            .with_main_axis_size(MainAxisSize::Min)
            .with_cross_axis_alignment(CrossAxisAlignment::Start)
            .with_spacing(ROW_INTERIOR_SPACING);

        row.add_child(title_row.finish());

        // When `display_name` overrides the alias, surface the alias
        // separately so the user can still see what they'd type at the
        // shell (and which key in `~/.ssh/config` this row corresponds
        // to).
        if metadata.and_then(|m| m.display_name.as_deref()).is_some() {
            row.add_child(
                ui.span(format!("alias: {}", host.alias))
                    .with_soft_wrap()
                    .build()
                    .finish(),
            );
        }

        if let Some(target) = format_target(host) {
            row.add_child(ui.span(target).with_soft_wrap().build().finish());
        }

        if !host.identity_files.is_empty() {
            row.add_child(
                ui.span(format!("Identity: {}", host.identity_files.join(", ")))
                    .with_soft_wrap()
                    .build()
                    .finish(),
            );
        }

        if let Some(proxy) = &host.proxy_jump {
            row.add_child(
                ui.span(format!("ProxyJump: {proxy}"))
                    .with_soft_wrap()
                    .build()
                    .finish(),
            );
        }

        if let Some(notes) = metadata.and_then(|m| m.notes.as_deref()) {
            row.add_child(ui.span(notes.to_string()).with_soft_wrap().build().finish());
        }

        if let Some(tags) = metadata
            .map(|m| m.tags.as_slice())
            .filter(|t| !t.is_empty())
        {
            row.add_child(render_tag_chips(tags, appearance));
        }

        Container::new(row.finish())
            .with_padding_top(ROW_PADDING)
            .with_padding_bottom(ROW_PADDING)
            .finish()
    })
    .with_cursor(Cursor::PointingHand)
    .on_click(move |ctx, _, _| {
        // Opens a fresh terminal tab (auto-focused as the active input)
        // and pre-fills `ssh <alias>` + submit so the connection starts
        // without a second keystroke. `AddTerminalTab` runs first, then
        // `RunCommand` is handled with the new tab as the active input
        // because both dispatches resolve within the same view update.
        ctx.dispatch_typed_action(WorkspaceAction::AddTerminalTab {
            hide_homepage: true,
        });
        ctx.dispatch_typed_action(WorkspaceAction::RunCommand(format!(
            "ssh {alias_for_dispatch}"
        )));
    })
    .finish();

    let edit_button = render_edit_button(alias_for_edit, edit_state, appearance);

    Container::new(
        Flex::row()
            .with_main_axis_alignment(MainAxisAlignment::SpaceBetween)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(connect_surface)
            .with_child(edit_button)
            .finish(),
    )
    .with_margin_bottom(ROW_SPACING - ROW_PADDING)
    .finish()
}

/// R3.5 — diagnostics row at the top of the page. Renders three
/// pill-style chips with the current `ssh-agent` health, the count
/// of visible identity files in `~/.ssh`, and a refresh button. The
/// chips' values come from cached probes on the view; the
/// [`SshHostsSettingsPageAction::RefreshDiagnostics`] action
/// re-runs the probes.
fn render_diagnostics_row(
    agent_status: &SshAgentStatus,
    identities: &[SshIdentity],
    refresh_state: MouseStateHandle,
    appearance: &Appearance,
) -> Box<dyn Element> {
    let agent_label = match agent_status {
        SshAgentStatus::Available { keys_loaded, .. } => {
            if *keys_loaded == 1 {
                "Agent: 1 key loaded".to_string()
            } else {
                format!("Agent: {keys_loaded} keys loaded")
            }
        }
        SshAgentStatus::NotConfigured => "Agent: not configured".to_string(),
        SshAgentStatus::Stale { .. } => "Agent: stale socket".to_string(),
    };

    let with_private = identities.iter().filter(|i| i.has_private_key).count();
    let total = identities.len();
    let identities_label = match (total, with_private) {
        (0, _) => "Keys in ~/.ssh: none visible".to_string(),
        (n, p) if n == p => format!("Keys in ~/.ssh: {n}"),
        (n, p) => format!("Keys in ~/.ssh: {n} ({} private-key-only)", n - p),
    };

    Container::new(
        Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_spacing(ACTIONS_ROW_SPACING)
            .with_child(render_diagnostics_chip(&agent_label, appearance))
            .with_child(render_diagnostics_chip(&identities_label, appearance))
            .with_child(render_action_button(
                "Refresh",
                refresh_state,
                appearance,
                SshHostsSettingsPageAction::RefreshDiagnostics,
            ))
            .finish(),
    )
    .with_margin_bottom(ACTIONS_ROW_BOTTOM_MARGIN)
    .finish()
}

/// Static text chip with the same visual treatment as the Edit /
/// Export / Import buttons but without click semantics.
fn render_diagnostics_chip(label: &str, appearance: &Appearance) -> Box<dyn Element> {
    let theme = appearance.theme();
    Container::new(
        appearance
            .ui_builder()
            .span(label.to_string())
            .build()
            .finish(),
    )
    .with_padding(Padding::uniform(EDIT_BUTTON_PADDING))
    .with_background(theme.surface_2())
    .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
    .with_border(Border::all(1.).with_border_fill(theme.outline()))
    .finish()
}

fn render_actions_row(
    export_state: MouseStateHandle,
    import_state: MouseStateHandle,
    appearance: &Appearance,
) -> Box<dyn Element> {
    Container::new(
        Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_spacing(ACTIONS_ROW_SPACING)
            .with_child(render_action_button(
                "Export\u{2026}",
                export_state,
                appearance,
                SshHostsSettingsPageAction::ExportMetadata,
            ))
            .with_child(render_action_button(
                "Import\u{2026}",
                import_state,
                appearance,
                SshHostsSettingsPageAction::ImportMetadata,
            ))
            .finish(),
    )
    .with_margin_bottom(ACTIONS_ROW_BOTTOM_MARGIN)
    .finish()
}

/// Page-level naked-style button rendered for the Export / Import
/// row. Identical visual treatment to the per-row edit button so the
/// chrome stays consistent.
fn render_action_button(
    label: &str,
    mouse_state: MouseStateHandle,
    appearance: &Appearance,
    action: SshHostsSettingsPageAction,
) -> Box<dyn Element> {
    let theme = appearance.theme();
    let surface_2 = theme.surface_2();
    let outline = theme.outline();
    let label = label.to_string();
    Hoverable::new(mouse_state, |_state| {
        Container::new(appearance.ui_builder().span(label).build().finish())
            .with_padding(Padding::uniform(EDIT_BUTTON_PADDING))
            .with_background(surface_2)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
            .with_border(Border::all(1.).with_border_fill(outline))
            .finish()
    })
    .with_cursor(Cursor::PointingHand)
    .on_click(move |ctx, _, _| {
        ctx.dispatch_typed_action(action.clone());
    })
    .finish()
}

fn emit_export_toast(
    path: &std::path::Path,
    result: Result<usize, String>,
    ctx: &mut ViewContext<SshHostsSettingsPageView>,
) {
    let window_id = ctx.window_id();
    let toast = match result {
        Ok(bytes) => DismissibleToast::success(format!(
            "Exported SSH host metadata to {} ({bytes} bytes).",
            path.display()
        )),
        Err(err) => DismissibleToast::error(format!("Export failed: {err}")),
    };
    ToastStack::handle(ctx).update(ctx, |stack, ctx| {
        stack.add_ephemeral_toast(toast, window_id, ctx);
    });
}

fn emit_import_toast(
    path: &std::path::Path,
    result: Result<ImportResult, String>,
    ctx: &mut ViewContext<SshHostsSettingsPageView>,
) {
    let window_id = ctx.window_id();
    let toast = match result {
        Ok(import) => {
            if import.total() == 0 {
                DismissibleToast::success(format!(
                    "Imported from {}: no host records found.",
                    path.display()
                ))
            } else {
                DismissibleToast::success(format!(
                    "Imported from {}: {} new, {} overwritten, {} unchanged.",
                    path.display(),
                    import.imported_new,
                    import.overwritten,
                    import.unchanged
                ))
            }
        }
        Err(err) => DismissibleToast::error(format!("Import failed: {err}")),
    };
    ToastStack::handle(ctx).update(ctx, |stack, ctx| {
        stack.add_ephemeral_toast(toast, window_id, ctx);
    });
}

/// "Edit" button rendered next to each host row. Dispatches a
/// page-level [`SshHostsSettingsPageAction::OpenMetadataEditor`]
/// rather than touching the dialog directly, because the click
/// closure only has `EventContext` (no view state).
fn render_edit_button(
    alias: String,
    mouse_state: MouseStateHandle,
    appearance: &Appearance,
) -> Box<dyn Element> {
    let theme = appearance.theme();
    let outline = theme.outline();
    let surface_2 = theme.surface_2();
    Hoverable::new(mouse_state, |_state| {
        Container::new(
            appearance
                .ui_builder()
                .span("Edit".to_string())
                .build()
                .finish(),
        )
        .with_padding(Padding::uniform(EDIT_BUTTON_PADDING))
        .with_background(surface_2)
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
        .with_border(Border::all(1.).with_border_fill(outline))
        .finish()
    })
    .with_cursor(Cursor::PointingHand)
    .on_click(move |ctx, _, _| {
        ctx.dispatch_typed_action(SshHostsSettingsPageAction::OpenMetadataEditor(
            alias.clone(),
        ));
    })
    .finish()
}

/// Render the per-host color accent. Returns `None` for unparseable
/// hex strings so a typo in the TOML degrades to "no swatch" rather
/// than crashing the page.
fn render_color_swatch(hex: &str, appearance: &Appearance) -> Option<Box<dyn Element>> {
    let color = coloru_from_hex_string(hex).ok()?;
    let swatch = Container::new(Flex::column().finish())
        .with_background(color)
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(
            COLOR_SWATCH_SIZE / 2.,
        )))
        .with_border(Border::all(1.).with_border_fill(appearance.theme().outline()))
        .finish();
    Some(
        ConstrainedBox::new(swatch)
            .with_width(COLOR_SWATCH_SIZE)
            .with_height(COLOR_SWATCH_SIZE)
            .finish(),
    )
}

/// Render the tag list as a wrap row of pill chips.
fn render_tag_chips(tags: &[String], appearance: &Appearance) -> Box<dyn Element> {
    let theme = appearance.theme();
    let chips: Vec<Box<dyn Element>> = tags
        .iter()
        .map(|tag| {
            Container::new(appearance.ui_builder().span(tag.clone()).build().finish())
                .with_padding(
                    Padding::uniform(CHIP_VERTICAL_PADDING)
                        .with_left(CHIP_HORIZONTAL_PADDING)
                        .with_right(CHIP_HORIZONTAL_PADDING),
                )
                .with_background(theme.surface_2())
                .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)))
                .with_border(Border::all(1.).with_border_fill(theme.outline()))
                .finish()
        })
        .collect();
    Container::new(
        Wrap::row()
            .with_run_spacing(CHIP_SPACING)
            .with_children(chips)
            .finish(),
    )
    .with_margin_top(CHIP_ROW_TOP_MARGIN)
    .finish()
}

/// Build the `user@hostname:port` subtitle from the optional fields. ssh
/// would fall back to its own defaults at connect time for anything
/// `None`, so we just omit those segments.
fn format_target(host: &HostDetail) -> Option<String> {
    let hostname = host.hostname.as_deref().unwrap_or(host.alias.as_str());
    let mut s = String::new();
    if let Some(user) = &host.user {
        s.push_str(user);
        s.push('@');
    }
    s.push_str(hostname);
    if let Some(port) = host.port {
        s.push(':');
        s.push_str(&port.to_string());
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
#[path = "ssh_hosts_tests.rs"]
mod tests;
