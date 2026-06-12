//! Per-host metadata overlay (display name, notes, tags, color) for
//! the SSH host management feature ([discussion #442](https://github.com/warpdotdev/warp/discussions/442)).
//!
//! Warp owns this metadata independently from `~/.ssh/config` — ssh
//! itself doesn't know about it. The store maps each ssh-config alias
//! to an optional [`HostMetadata`] record. Records are persisted to
//! `~/.warp/ssh_hosts_metadata.toml` ([`warp_core::paths::warp_home_ssh_hosts_metadata_file_path`])
//! and reloaded on startup.
//!
//! Cross-device sync (Phase 1.6) is offered as `export_to_path` /
//! `import_from_path` — users move the TOML between machines via their
//! own tool of choice (Dropbox, iCloud, syncthing, git, scp). Server-
//! backed Warp Drive sync was scoped out because adding a new
//! `JsonObjectType` requires a coordinated server schema change; the
//! `HostMetadata` struct stays a plain `Serialize`/`Deserialize` so
//! a future PR can wrap it in a `cloud_object` model without any
//! on-disk layout churn.

use std::collections::HashMap;
#[cfg(not(target_family = "wasm"))]
use std::collections::HashSet;
#[cfg(not(target_family = "wasm"))]
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
#[cfg(not(target_family = "wasm"))]
use warp_ssh_config::HostDetail;
use warpui::{Entity, ModelContext, SingletonEntity};

#[cfg(not(target_family = "wasm"))]
use crate::ssh_hosts::{SshHostsEvent, SshHostsModel};

/// User-authored metadata for one SSH host alias.
///
/// Empty values are skipped during serialization, so a host with no
/// metadata never appears in the TOML at all (see [`Self::is_empty`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMetadata {
    /// Override the alias shown in the host list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Free-text notes the user writes about this host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// User-assigned tags. Filter chips planned for a follow-up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Color accent as a CSS-style hex string (e.g. `#ff5733`). The
    /// value is stored verbatim; validation is the UI's responsibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl HostMetadata {
    /// `true` when no fields are set. Used to garbage-collect the
    /// on-disk map: empty records are removed rather than persisted
    /// as `{}`.
    pub fn is_empty(&self) -> bool {
        self.display_name.is_none()
            && self.notes.is_none()
            && self.tags.is_empty()
            && self.color.is_none()
    }
}

/// On-disk TOML layout. Nested under `[hosts.<alias>]` so we have
/// room to add top-level keys later without churning the per-host
/// schema.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MetadataFile {
    #[serde(default)]
    hosts: HashMap<String, HostMetadata>,
}

/// Events the [`SshHostsMetadataModel`] emits.
pub enum SshHostsMetadataEvent {
    /// Metadata for one or more aliases changed. Subscribers should
    /// re-read via [`SshHostsMetadataModel::get`] for the current
    /// snapshot — no diff payload is carried because UI consumers
    /// generally re-render the full list anyway.
    Updated,
}

/// Counters returned from [`SshHostsMetadataModel::import_from_path`]
/// so the UI can render a meaningful toast (e.g. "Imported 3 new, 2
/// overwritten, 4 unchanged").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportResult {
    /// Aliases that didn't have any prior record.
    pub imported_new: usize,
    /// Aliases whose record differed from the existing one and were
    /// overwritten by the imported value (imported-wins policy).
    pub overwritten: usize,
    /// Aliases whose imported record matched the existing record
    /// exactly — no-op, no write.
    pub unchanged: usize,
}

impl ImportResult {
    /// Total records read from the imported file.
    pub fn total(&self) -> usize {
        self.imported_new + self.overwritten + self.unchanged
    }

    /// `true` when at least one record was added or modified.
    pub fn touched_disk(&self) -> bool {
        self.imported_new + self.overwritten > 0
    }
}

/// In-memory cache of per-host metadata. Singleton, registered at
/// app startup after [`SshHostsModel`] so the prune-on-config-change
/// subscription has a model to listen to.
pub struct SshHostsMetadataModel {
    by_alias: HashMap<String, HostMetadata>,
    /// Resolved path to the TOML store. `None` when `$HOME` is unset
    /// (no IO is performed in that case).
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    path: Option<PathBuf>,
}

impl SshHostsMetadataModel {
    #[cfg(not(target_family = "wasm"))]
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        Self::new_internal(ctx, true)
    }

    #[cfg(target_family = "wasm")]
    pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
        Self {
            by_alias: HashMap::new(),
            path: None,
        }
    }

    /// Test-only constructor — skips disk IO and the
    /// [`SshHostsModel`] subscription so unit tests don't depend on
    /// global state.
    #[cfg(all(test, not(target_family = "wasm")))]
    #[allow(dead_code)]
    pub fn new_for_testing(_ctx: &mut ModelContext<Self>) -> Self {
        Self {
            by_alias: HashMap::new(),
            path: None,
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn new_internal(ctx: &mut ModelContext<Self>, load_from_disk: bool) -> Self {
        let path = warp_core::paths::warp_home_ssh_hosts_metadata_file_path();
        let by_alias = if load_from_disk {
            path.as_deref().map(load_from_path).unwrap_or_default()
        } else {
            HashMap::new()
        };

        let ssh_hosts = SshHostsModel::handle(ctx);
        ctx.subscribe_to_model(&ssh_hosts, |me, event, ctx| match event {
            SshHostsEvent::HostsUpdated(new_hosts) => {
                me.prune_to_known_aliases(new_hosts, ctx);
            }
        });

        Self { by_alias, path }
    }

    /// Snapshot of the metadata for `alias`, or `None` when the user
    /// has set no fields for this host.
    pub fn get(&self, alias: &str) -> Option<&HostMetadata> {
        self.by_alias.get(alias)
    }

    /// Set the metadata for `alias`. An empty record is treated as
    /// "remove" so the on-disk store stays compact.
    pub fn set(&mut self, alias: String, metadata: HostMetadata, ctx: &mut ModelContext<Self>) {
        let changed = if metadata.is_empty() {
            self.by_alias.remove(&alias).is_some()
        } else {
            let old = self.by_alias.insert(alias, metadata.clone());
            old.as_ref() != Some(&metadata)
        };
        if changed {
            self.persist();
            ctx.emit(SshHostsMetadataEvent::Updated);
        }
    }

    /// Serialize the current metadata map to a TOML file at `path`.
    /// Returns the byte count written on success. Used by Phase 1.6
    /// "Export…" so users can sync metadata across machines via their
    /// own file-sync tool of choice (Dropbox, iCloud Drive, syncthing,
    /// git, scp). Does not mutate in-memory state.
    #[cfg(not(target_family = "wasm"))]
    pub fn export_to_path(&self, path: &Path) -> Result<usize, String> {
        let file = MetadataFile {
            hosts: self.by_alias.clone(),
        };
        let serialized = toml::to_string_pretty(&file)
            .map_err(|e| format!("Failed to serialize metadata: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Failed to create parent directory {}: {e}",
                    parent.display()
                )
            })?;
        }
        let bytes = serialized.len();
        std::fs::write(path, serialized)
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
        Ok(bytes)
    }

    /// Read a TOML file at `path`, merge into the current metadata
    /// map (imported entries overwrite existing ones for the same
    /// alias — imported-wins last-write semantics), persist, and emit
    /// `Updated` if anything changed.
    ///
    /// Counts breakdown surfaces in the toast via [`ImportResult`].
    #[cfg(not(target_family = "wasm"))]
    pub fn import_from_path(
        &mut self,
        path: &Path,
        ctx: &mut ModelContext<Self>,
    ) -> Result<ImportResult, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        let parsed: MetadataFile = toml::from_str(&content)
            .map_err(|e| format!("Failed to parse {} as TOML: {e}", path.display()))?;

        let result = merge_imported(&mut self.by_alias, parsed.hosts);
        if result.touched_disk() {
            self.persist();
            ctx.emit(SshHostsMetadataEvent::Updated);
        }
        Ok(result)
    }

    /// Drop any metadata record whose alias no longer appears in
    /// `~/.ssh/config`. Called from the [`SshHostsModel`] subscription
    /// so renames / removes don't leave orphan records.
    #[cfg(not(target_family = "wasm"))]
    fn prune_to_known_aliases(&mut self, hosts: &[HostDetail], ctx: &mut ModelContext<Self>) {
        let known: HashSet<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        let before = self.by_alias.len();
        self.by_alias.retain(|k, _| known.contains(k.as_str()));
        if self.by_alias.len() != before {
            self.persist();
            ctx.emit(SshHostsMetadataEvent::Updated);
        }
    }

    /// Write the in-memory map to disk. Failures are logged but never
    /// propagated — the in-memory state stays authoritative for the
    /// session, and the next mutation will retry the write.
    #[cfg(not(target_family = "wasm"))]
    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let file = MetadataFile {
            hosts: self.by_alias.clone(),
        };
        match toml::to_string_pretty(&file) {
            Ok(serialized) => {
                if let Some(parent) = path.parent() {
                    if let Err(err) = std::fs::create_dir_all(parent) {
                        log::warn!(
                            "Failed to ensure SSH host metadata parent dir {}: {err}",
                            parent.display()
                        );
                        return;
                    }
                }
                if let Err(err) = std::fs::write(path, serialized) {
                    log::warn!(
                        "Failed to write SSH host metadata to {}: {err}",
                        path.display()
                    );
                }
            }
            Err(err) => {
                log::warn!("Failed to serialize SSH host metadata: {err}");
            }
        }
    }
}

impl Entity for SshHostsMetadataModel {
    type Event = SshHostsMetadataEvent;
}

impl SingletonEntity for SshHostsMetadataModel {}

/// Merge `imported` into `current` using imported-wins semantics, and
/// report counts. Pure function so unit tests can drive it directly
/// without needing a `ModelContext`.
fn merge_imported(
    current: &mut HashMap<String, HostMetadata>,
    imported: HashMap<String, HostMetadata>,
) -> ImportResult {
    let mut result = ImportResult::default();
    for (alias, metadata) in imported {
        match current.get(&alias) {
            Some(existing) if existing == &metadata => {
                result.unchanged += 1;
            }
            Some(_) => {
                current.insert(alias, metadata);
                result.overwritten += 1;
            }
            None => {
                current.insert(alias, metadata);
                result.imported_new += 1;
            }
        }
    }
    result
}

#[cfg(not(target_family = "wasm"))]
fn load_from_path(path: &Path) -> HashMap<String, HostMetadata> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(err) => {
            log::warn!(
                "Failed to read SSH host metadata from {}: {err}",
                path.display()
            );
            return HashMap::new();
        }
    };
    match toml::from_str::<MetadataFile>(&content) {
        Ok(file) => file.hosts,
        Err(err) => {
            log::warn!(
                "Failed to parse SSH host metadata at {}: {err}",
                path.display()
            );
            HashMap::new()
        }
    }
}

#[cfg(test)]
#[path = "ssh_hosts_metadata_tests.rs"]
mod tests;
