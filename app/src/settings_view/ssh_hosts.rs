//! Settings → SSH hosts page (read-only view).
//!
//! Phase 1.3 of the SSH host management feature
//! ([discussion #442](https://github.com/warpdotdev/warp/discussions/442)).
//! Surfaces the host aliases parsed by [`crate::ssh_hosts::SshHostsModel`]
//! from `~/.ssh/config` as a Settings page. Each entry shows the alias
//! plus the resolved connect-time metadata ssh would use (hostname,
//! user, port, identity files, ProxyJump).
//!
//! No connect-time actions yet — that's Phase 1.4. Live-refreshes via
//! the model's `HostsUpdated` event whenever the parsed config changes
//! on disk.

use warp_ssh_config::HostDetail;
use warpui::elements::{Container, CrossAxisAlignment, Element, Flex, MainAxisSize, ParentElement};
use warpui::ui_components::components::UiComponent;
use warpui::{AppContext, Entity, SingletonEntity, View, ViewContext, ViewHandle};

use super::settings_page::{
    MatchData, PageType, SettingsPageEvent, SettingsPageMeta, SettingsPageViewHandle,
    SettingsWidget,
};
use super::SettingsSection;
use crate::appearance::Appearance;
use crate::ssh_hosts::{SshHostsEvent, SshHostsModel};

const ROW_SPACING: f32 = 12.;
const ROW_INTERIOR_SPACING: f32 = 2.;
const DESCRIPTION_BOTTOM_MARGIN: f32 = 12.;

pub struct SshHostsSettingsPageView {
    page: PageType<Self>,
}

impl SshHostsSettingsPageView {
    pub fn new(ctx: &mut ViewContext<SshHostsSettingsPageView>) -> Self {
        // Re-render when the parsed host list changes on disk.
        let model = SshHostsModel::handle(ctx);
        ctx.subscribe_to_model(&model, |_, _, event, ctx| match event {
            SshHostsEvent::HostsUpdated(_) => {
                ctx.notify();
            }
        });

        Self {
            page: PageType::new_monolith(SshHostsWidget, Some("SSH hosts"), false),
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
        _view: &SshHostsSettingsPageView,
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
            for host in hosts {
                column.add_child(render_host_row(host, appearance));
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

fn render_host_row(host: &HostDetail, appearance: &Appearance) -> Box<dyn Element> {
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
        .with_margin_bottom(ROW_SPACING)
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
