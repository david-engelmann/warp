//! Settings → SSH hosts page.
//!
//! Surfaces the host aliases parsed by [`crate::ssh_hosts::SshHostsModel`]
//! from `~/.ssh/config` as a Settings page (Phase 1.3 of
//! [discussion #442](https://github.com/warpdotdev/warp/discussions/442)).
//! Each entry shows the alias plus the resolved connect-time metadata
//! ssh would use (hostname, user, port, identity files, ProxyJump).
//!
//! Rows are click-to-connect (Phase 1.4): a click opens a fresh
//! terminal tab and submits `ssh <alias>` in one go. The page
//! live-refreshes via the model's `HostsUpdated` event whenever the
//! parsed config changes on disk.

use warp_ssh_config::HostDetail;
use warpui::elements::{
    Container, CrossAxisAlignment, Element, Flex, Hoverable, MainAxisSize, MouseStateHandle,
    ParentElement,
};
use warpui::platform::Cursor;
use warpui::ui_components::components::UiComponent;
use warpui::{AppContext, Entity, SingletonEntity, View, ViewContext, ViewHandle};

use super::settings_page::{
    MatchData, PageType, SettingsPageEvent, SettingsPageMeta, SettingsPageViewHandle,
    SettingsWidget,
};
use super::SettingsSection;
use crate::appearance::Appearance;
use crate::ssh_hosts::{SshHostsEvent, SshHostsModel};
use crate::workspace::WorkspaceAction;

const ROW_SPACING: f32 = 12.;
const ROW_INTERIOR_SPACING: f32 = 2.;
const DESCRIPTION_BOTTOM_MARGIN: f32 = 12.;
const ROW_PADDING: f32 = 8.;

pub struct SshHostsSettingsPageView {
    page: PageType<Self>,
    /// Per-row mouse-state handles, indexed in the same order as the
    /// host list snapshot held by [`SshHostsModel`]. Kept in sync with
    /// the host list via the model subscription.
    row_states: Vec<MouseStateHandle>,
}

impl SshHostsSettingsPageView {
    pub fn new(ctx: &mut ViewContext<SshHostsSettingsPageView>) -> Self {
        // Re-render when the parsed host list changes on disk, and
        // resize the per-row handle vector to match.
        let model = SshHostsModel::handle(ctx);
        ctx.subscribe_to_model(&model, |me, _, event, ctx| match event {
            SshHostsEvent::HostsUpdated(new_hosts) => {
                me.row_states
                    .resize_with(new_hosts.len(), MouseStateHandle::default);
                ctx.notify();
            }
        });

        let initial_count = SshHostsModel::as_ref(ctx).hosts().len();
        let row_states = (0..initial_count).map(|_| Default::default()).collect();

        Self {
            page: PageType::new_monolith(SshHostsWidget, Some("SSH hosts"), false),
            row_states,
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
        let mut column = Flex::column()
            .with_main_axis_size(MainAxisSize::Min)
            .with_cross_axis_alignment(CrossAxisAlignment::Stretch);

        column.add_child(render_description(appearance));

        if hosts.is_empty() {
            column.add_child(render_empty_state(appearance));
        } else {
            for (idx, host) in hosts.iter().enumerate() {
                // Fall back to a fresh handle if `row_states` is briefly
                // out of sync with the host list (the resize fires on
                // the next `HostsUpdated` after a config edit).
                let mouse_state = view.row_states.get(idx).cloned().unwrap_or_default();
                column.add_child(render_host_row(host, mouse_state, appearance));
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
    mouse_state: MouseStateHandle,
    appearance: &Appearance,
) -> Box<dyn Element> {
    // Captured by the click handler — needs to outlive the closure body.
    let alias_for_dispatch = host.alias.clone();

    Hoverable::new(mouse_state, |_state| {
        let ui = appearance.ui_builder();
        let mut row = Flex::column()
            .with_main_axis_size(MainAxisSize::Min)
            .with_cross_axis_alignment(CrossAxisAlignment::Start)
            .with_spacing(ROW_INTERIOR_SPACING);

        row.add_child(ui.span(host.alias.clone()).build().finish());

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

        Container::new(row.finish())
            .with_padding_top(ROW_PADDING)
            .with_padding_bottom(ROW_PADDING)
            .with_margin_bottom(ROW_SPACING - ROW_PADDING)
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
