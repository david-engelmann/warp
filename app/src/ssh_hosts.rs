//! Data layer for the SSH host management feature
//! ([Discussion #442](https://github.com/warpdotdev/warp/discussions/442)).
//!
//! Keeps a parsed view of the user's `~/.ssh/config` in memory, served
//! to UI consumers as a [`SshHostsModel`]. The model re-parses the
//! config whenever a file in `~/.ssh/` changes on disk and emits
//! [`SshHostsEvent::HostsUpdated`] when the resulting host list
//! differs.
//!
//! Phase 1.2 — data layer only. UI surfaces (settings page,
//! one-click connect, command palette) consume this model in
//! subsequent phases.

#[cfg(not(target_family = "wasm"))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(target_family = "wasm"))]
use std::sync::Arc;
#[cfg(not(target_family = "wasm"))]
use std::time::Duration;

#[cfg(not(target_family = "wasm"))]
use dirs::home_dir;
#[cfg(not(target_family = "wasm"))]
use notify_debouncer_full::notify::{RecursiveMode, WatchFilter};
use warp_ssh_config::HostDetail;
#[cfg(not(target_family = "wasm"))]
use warpui::ModelHandle;
use warpui::{Entity, ModelContext, SingletonEntity};
#[cfg(not(target_family = "wasm"))]
use watcher::{BulkFilesystemWatcher, BulkFilesystemWatcherEvent};

/// Debounce window for `~/.ssh/` change events, in milliseconds. Long
/// enough to coalesce editor save-burst writes (e.g. vim's
/// write-temp-then-rename dance) into a single refresh.
#[cfg(not(target_family = "wasm"))]
const SSH_HOSTS_WATCHER_DEBOUNCE_MILLI_SECS: u64 = 500;

/// Events [`SshHostsModel`] subscribers can listen for.
pub enum SshHostsEvent {
    /// The parsed host list changed. Payload is the new full list,
    /// alphabetically sorted by alias. Subscribers that re-read via
    /// [`SshHostsModel::hosts`] can ignore the payload; it's carried
    /// here for callers that prefer not to chase the model handle.
    HostsUpdated(#[allow(dead_code)] Vec<HostDetail>),
}

/// In-memory cache of the SSH hosts named in `~/.ssh/config`.
///
/// Registered as a singleton at app startup. UI code should read
/// [`SshHostsModel::hosts`] for the current snapshot and subscribe to
/// the model for change notifications.
pub struct SshHostsModel {
    hosts: Vec<HostDetail>,
    /// Filesystem watcher kept alive for the lifetime of the model.
    /// Dropping this stops watching the config directory.
    #[cfg(not(target_family = "wasm"))]
    _watcher: Option<ModelHandle<BulkFilesystemWatcher>>,
}

impl SshHostsModel {
    #[cfg(not(target_family = "wasm"))]
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        Self::new_internal(ctx, true)
    }

    #[cfg(target_family = "wasm")]
    pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
        Self { hosts: Vec::new() }
    }

    /// Test-only constructor — skips watcher registration and starts
    /// with an empty host list, so unit tests don't depend on the
    /// caller's real `~/.ssh/config`.
    #[cfg(all(test, not(target_family = "wasm")))]
    #[allow(dead_code)]
    pub fn new_for_testing(_ctx: &mut ModelContext<Self>) -> Self {
        Self {
            hosts: Vec::new(),
            _watcher: None,
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn new_internal(ctx: &mut ModelContext<Self>, register_watcher: bool) -> Self {
        let hosts = warp_ssh_config::list_user_ssh_host_details();
        let watcher = if register_watcher {
            register_ssh_dir_watcher(ctx)
        } else {
            None
        };
        Self {
            hosts,
            _watcher: watcher,
        }
    }

    /// Snapshot of the current host list. Alphabetically sorted by
    /// alias; deduplicated on alias.
    pub fn hosts(&self) -> &[HostDetail] {
        &self.hosts
    }

    /// Apply a freshly-parsed host list. Emits
    /// [`SshHostsEvent::HostsUpdated`] iff the list changed.
    #[cfg(not(target_family = "wasm"))]
    fn apply_refresh(&mut self, new_hosts: Vec<HostDetail>, ctx: &mut ModelContext<Self>) {
        if new_hosts == self.hosts {
            return;
        }
        self.hosts = new_hosts.clone();
        ctx.emit(SshHostsEvent::HostsUpdated(new_hosts));
    }

    #[cfg(not(target_family = "wasm"))]
    fn handle_fs_event(
        &mut self,
        _event: &BulkFilesystemWatcherEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        // The watcher's emit filter already narrows events to config
        // files; any event reaching us is a signal to re-parse.
        let new_hosts = warp_ssh_config::list_user_ssh_host_details();
        self.apply_refresh(new_hosts, ctx);
    }
}

impl Entity for SshHostsModel {
    type Event = SshHostsEvent;
}

impl SingletonEntity for SshHostsModel {}

/// `~/.ssh/` if `$HOME` is set.
#[cfg(not(target_family = "wasm"))]
fn user_ssh_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".ssh"))
}

#[cfg(not(target_family = "wasm"))]
fn register_ssh_dir_watcher(
    ctx: &mut ModelContext<SshHostsModel>,
) -> Option<ModelHandle<BulkFilesystemWatcher>> {
    let ssh_dir = user_ssh_dir()?;
    if !ssh_dir.exists() {
        // No directory to watch — the user simply hasn't created one.
        // The initial parse already returned an empty list; nothing
        // more to do until the directory appears.
        return None;
    }

    let watcher = ctx.add_model(|ctx| {
        BulkFilesystemWatcher::new(
            Duration::from_millis(SSH_HOSTS_WATCHER_DEBOUNCE_MILLI_SECS),
            ctx,
        )
    });
    ctx.subscribe_to_model(&watcher, SshHostsModel::handle_fs_event);

    let emit_dir = ssh_dir.clone();
    let emit_filter: Arc<dyn Fn(&Path) -> bool + Send + Sync> =
        Arc::new(move |path: &Path| is_ssh_config_path(path, &emit_dir));
    // Walk filter accepts everything so we descend into `config.d/`
    // and similar sub-folders users sometimes Include from.
    let watch_filter = WatchFilter::with_filter(Arc::new(|_: &Path| true), emit_filter);

    let registration_path = ssh_dir.clone();
    let registration = watcher.update(ctx, |watcher, _ctx| {
        watcher.register_path(&registration_path, watch_filter, RecursiveMode::Recursive)
    });
    let log_path = ssh_dir;
    ctx.spawn(registration, move |_, result, _ctx| {
        if let Err(err) = result {
            log::warn!(
                "Failed to start watching SSH config directory {}: {err}",
                log_path.display()
            );
        }
    });

    Some(watcher)
}

/// Heuristic: does `path` look like an ssh_config file that, if
/// changed, should trigger a re-parse?
///
/// True for `~/.ssh/config` itself, any file ending in `.conf` /
/// `.config` under `~/.ssh/`, and anything inside a `config.d/`
/// subdirectory (a common convention for split configs people
/// `Include` from the main file).
///
/// Conservative on the false-positive side: a stray refresh is
/// cheap (re-parse + diff), a missed refresh is a stale UI.
#[cfg(not(target_family = "wasm"))]
fn is_ssh_config_path(path: &Path, ssh_dir: &Path) -> bool {
    if !path.starts_with(ssh_dir) {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name == "config" {
        return true;
    }
    if name.ends_with(".conf") || name.ends_with(".config") {
        return true;
    }
    if path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|p| p == "config.d" || p == "conf.d")
    {
        return true;
    }
    false
}

#[cfg(test)]
#[path = "ssh_hosts_tests.rs"]
mod tests;
