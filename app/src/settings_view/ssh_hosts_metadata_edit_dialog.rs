//! Modal dialog for editing per-host metadata
//! ([discussion #442](https://github.com/warpdotdev/warp/discussions/442),
//! Phase 1.5b).
//!
//! Four single-line editors map to the [`HostMetadata`] fields:
//! `display_name`, `notes`, `tags` (comma-separated), and `color`
//! (hex string). Save dispatches the parsed metadata back to the
//! page via [`SshHostsMetadataEditDialogEvent::Save`]; the page is
//! responsible for persisting via
//! [`crate::ssh_hosts_metadata::SshHostsMetadataModel::set`].

use warpui::elements::{
    ChildView, Container, CrossAxisAlignment, Dismiss, Element, Empty, Flex, ParentElement,
};
use warpui::ui_components::components::UiComponent;
use warpui::{AppContext, Entity, SingletonEntity, TypedActionView, View, ViewContext, ViewHandle};

use crate::appearance::Appearance;
use crate::editor::{
    EditorView, PropagateAndNoOpNavigationKeys, SingleLineEditorOptions, TextOptions,
};
use crate::ssh_hosts_metadata::HostMetadata;
use crate::ui_components::dialog::{dialog_styles, Dialog};
use crate::view_components::action_button::{ActionButton, NakedTheme, PrimaryTheme};

const DIALOG_WIDTH: f32 = 480.;
const FIELD_SPACING: f32 = 12.;
const LABEL_TO_INPUT_SPACING: f32 = 4.;

/// Emitted to the parent page once the user resolves the dialog.
pub enum SshHostsMetadataEditDialogEvent {
    /// User clicked Save (or pressed Enter on the form). The parent
    /// should call `SshHostsMetadataModel::set(alias, metadata)` and
    /// hide the dialog.
    Save {
        alias: String,
        metadata: HostMetadata,
    },
    /// User clicked Cancel, dismissed via Esc, or clicked the
    /// backdrop.
    Cancel,
}

#[derive(Debug)]
pub enum SshHostsMetadataEditDialogAction {
    Cancel,
    Save,
}

pub struct SshHostsMetadataEditDialog {
    visible: bool,
    /// The ssh_config alias being edited. Set by [`Self::show`]; used
    /// as the dispatch key when emitting `Save`.
    alias: Option<String>,
    display_name_editor: ViewHandle<EditorView>,
    notes_editor: ViewHandle<EditorView>,
    tags_editor: ViewHandle<EditorView>,
    color_editor: ViewHandle<EditorView>,
    cancel_button: ViewHandle<ActionButton>,
    save_button: ViewHandle<ActionButton>,
}

impl SshHostsMetadataEditDialog {
    pub fn new(ctx: &mut ViewContext<Self>) -> Self {
        let font_family = Appearance::as_ref(ctx).ui_font_family();
        let text_colors = crate::settings_view::editor_text_colors(Appearance::as_ref(ctx));

        let make_editor = |placeholder: &'static str, ctx: &mut ViewContext<Self>| {
            let text_colors = text_colors.clone();
            ctx.add_typed_action_view(move |ctx| {
                let options = SingleLineEditorOptions {
                    text: TextOptions {
                        font_family_override: Some(font_family),
                        text_colors_override: Some(text_colors.clone()),
                        ..Default::default()
                    },
                    propagate_and_no_op_vertical_navigation_keys:
                        PropagateAndNoOpNavigationKeys::Always,
                    ..Default::default()
                };
                let mut editor = EditorView::single_line(options, ctx);
                editor.set_placeholder_text(placeholder, ctx);
                editor
            })
        };

        let display_name_editor = make_editor("Custom name shown instead of the alias", ctx);
        let notes_editor = make_editor("Free-text notes about this host", ctx);
        let tags_editor = make_editor("Comma-separated tags, e.g. prod, us-east", ctx);
        let color_editor = make_editor("Hex color, e.g. #ff5733", ctx);

        let cancel_button = ctx.add_typed_action_view(|_| {
            ActionButton::new("Cancel", NakedTheme).on_click(|ctx| {
                ctx.dispatch_typed_action(SshHostsMetadataEditDialogAction::Cancel);
            })
        });

        let save_button = ctx.add_typed_action_view(|_| {
            ActionButton::new("Save", PrimaryTheme).on_click(|ctx| {
                ctx.dispatch_typed_action(SshHostsMetadataEditDialogAction::Save);
            })
        });

        Self {
            visible: false,
            alias: None,
            display_name_editor,
            notes_editor,
            tags_editor,
            color_editor,
            cancel_button,
            save_button,
        }
    }

    /// Open the dialog, populating the form fields with the current
    /// metadata for `alias` (or empty strings when no record exists).
    pub fn show(
        &mut self,
        alias: String,
        metadata: Option<&HostMetadata>,
        ctx: &mut ViewContext<Self>,
    ) {
        let display_name = metadata
            .and_then(|m| m.display_name.clone())
            .unwrap_or_default();
        let notes = metadata.and_then(|m| m.notes.clone()).unwrap_or_default();
        let tags = metadata.map(|m| m.tags.join(", ")).unwrap_or_default();
        let color = metadata.and_then(|m| m.color.clone()).unwrap_or_default();

        // Avoid borrowing self mutably across the ctx.update() calls
        // below by cloning the handles up front.
        let display_name_editor = self.display_name_editor.clone();
        let notes_editor = self.notes_editor.clone();
        let tags_editor = self.tags_editor.clone();
        let color_editor = self.color_editor.clone();

        display_name_editor.update(ctx, |e, ctx| e.set_buffer_text(&display_name, ctx));
        notes_editor.update(ctx, |e, ctx| e.set_buffer_text(&notes, ctx));
        tags_editor.update(ctx, |e, ctx| e.set_buffer_text(&tags, ctx));
        color_editor.update(ctx, |e, ctx| e.set_buffer_text(&color, ctx));

        self.alias = Some(alias);
        self.visible = true;
        ctx.notify();
    }

    pub fn hide(&mut self, ctx: &mut ViewContext<Self>) {
        self.visible = false;
        self.alias = None;
        ctx.notify();
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Read the editor buffers + assemble a [`HostMetadata`] record.
    fn collect_metadata(&self, ctx: &ViewContext<Self>) -> HostMetadata {
        let display_name = trimmed_or_none(&self.display_name_editor.as_ref(ctx).buffer_text(ctx));
        let notes = trimmed_or_none(&self.notes_editor.as_ref(ctx).buffer_text(ctx));
        let tags = parse_tag_list(&self.tags_editor.as_ref(ctx).buffer_text(ctx));
        let color = trimmed_or_none(&self.color_editor.as_ref(ctx).buffer_text(ctx));

        HostMetadata {
            display_name,
            notes,
            tags,
            color,
        }
    }
}

impl Entity for SshHostsMetadataEditDialog {
    type Event = SshHostsMetadataEditDialogEvent;
}

impl View for SshHostsMetadataEditDialog {
    fn ui_name() -> &'static str {
        "SshHostsMetadataEditDialog"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        if !self.visible {
            return Empty::new().finish();
        }

        let appearance = Appearance::as_ref(app);

        let body = Flex::column()
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
            .with_spacing(FIELD_SPACING)
            .with_child(render_field(
                "Display name",
                &self.display_name_editor,
                appearance,
            ))
            .with_child(render_field("Notes", &self.notes_editor, appearance))
            .with_child(render_field(
                "Tags (comma-separated)",
                &self.tags_editor,
                appearance,
            ))
            .with_child(render_field(
                "Color (hex, e.g. #ff5733)",
                &self.color_editor,
                appearance,
            ))
            .finish();

        let title = self
            .alias
            .as_deref()
            .map(|alias| format!("Edit metadata · {alias}"))
            .unwrap_or_else(|| "Edit metadata".to_string());

        let dialog = Dialog::new(title, None, dialog_styles(appearance))
            .with_child(body)
            .with_bottom_row_child(ChildView::new(&self.cancel_button).finish())
            .with_bottom_row_child(
                Container::new(ChildView::new(&self.save_button).finish())
                    .with_margin_left(12.)
                    .finish(),
            )
            .with_width(DIALOG_WIDTH)
            .build()
            .finish();

        Dismiss::new(dialog)
            .prevent_interaction_with_other_elements()
            .on_dismiss(|ctx, _app| {
                ctx.dispatch_typed_action(SshHostsMetadataEditDialogAction::Cancel)
            })
            .finish()
    }
}

impl TypedActionView for SshHostsMetadataEditDialog {
    type Action = SshHostsMetadataEditDialogAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            SshHostsMetadataEditDialogAction::Cancel => {
                ctx.emit(SshHostsMetadataEditDialogEvent::Cancel);
            }
            SshHostsMetadataEditDialogAction::Save => {
                let Some(alias) = self.alias.clone() else {
                    // Defensive — Save shouldn't fire without an open
                    // dialog, but if it does, treat as Cancel.
                    ctx.emit(SshHostsMetadataEditDialogEvent::Cancel);
                    return;
                };
                let metadata = self.collect_metadata(ctx);
                ctx.emit(SshHostsMetadataEditDialogEvent::Save { alias, metadata });
            }
        }
    }
}

fn render_field(
    label: &str,
    editor: &ViewHandle<EditorView>,
    appearance: &Appearance,
) -> Box<dyn Element> {
    Flex::column()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_spacing(LABEL_TO_INPUT_SPACING)
        .with_child(
            appearance
                .ui_builder()
                .span(label.to_string())
                .build()
                .finish(),
        )
        .with_child(ChildView::new(editor).finish())
        .finish()
}

fn trimmed_or_none(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Parse a comma-separated tag list. Whitespace around each entry is
/// trimmed; empty entries are dropped; duplicates preserved in order.
fn parse_tag_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
#[path = "ssh_hosts_metadata_edit_dialog_tests.rs"]
mod tests;
