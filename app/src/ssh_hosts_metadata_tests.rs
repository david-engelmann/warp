use std::collections::HashMap;

use super::{load_from_path, merge_imported, HostMetadata, ImportResult, MetadataFile};

#[test]
fn host_metadata_default_is_empty() {
    let m = HostMetadata::default();
    assert!(m.is_empty());
}

#[test]
fn host_metadata_with_display_name_is_not_empty() {
    let m = HostMetadata {
        display_name: Some("Prod web".to_string()),
        ..Default::default()
    };
    assert!(!m.is_empty());
}

#[test]
fn host_metadata_with_only_tags_is_not_empty() {
    let m = HostMetadata {
        tags: vec!["prod".to_string()],
        ..Default::default()
    };
    assert!(!m.is_empty());
}

#[test]
fn host_metadata_with_only_color_is_not_empty() {
    let m = HostMetadata {
        color: Some("#ff5733".to_string()),
        ..Default::default()
    };
    assert!(!m.is_empty());
}

#[test]
fn toml_roundtrip_preserves_all_fields() {
    let mut hosts = HashMap::new();
    hosts.insert(
        "prod-web".to_string(),
        HostMetadata {
            display_name: Some("Prod webserver".to_string()),
            notes: Some("Use --max-time 30".to_string()),
            tags: vec!["prod".to_string(), "us-east".to_string()],
            color: Some("#ff5733".to_string()),
        },
    );
    hosts.insert(
        "staging".to_string(),
        HostMetadata {
            tags: vec!["staging".to_string()],
            ..Default::default()
        },
    );

    let file = MetadataFile { hosts };
    let serialized = toml::to_string_pretty(&file).expect("serialize");
    let parsed: MetadataFile = toml::from_str(&serialized).expect("deserialize");

    assert_eq!(parsed.hosts, file.hosts);
}

#[test]
fn empty_optional_fields_omitted_from_serialized_toml() {
    // skip_serializing_if guards keep the file compact — a host with
    // only `tags` set must NOT serialize `display_name = ""` etc.
    let mut hosts = HashMap::new();
    hosts.insert(
        "minimal".to_string(),
        HostMetadata {
            tags: vec!["dev".to_string()],
            ..Default::default()
        },
    );

    let serialized = toml::to_string_pretty(&MetadataFile { hosts }).expect("serialize");

    assert!(!serialized.contains("display_name"));
    assert!(!serialized.contains("notes"));
    assert!(!serialized.contains("color"));
    assert!(serialized.contains("tags"));
}

#[test]
fn load_from_missing_path_returns_empty_map_silently() {
    // The metadata file is optional — a fresh install hasn't created
    // one yet, and that must NOT fail loudly.
    let path = std::env::temp_dir().join("ssh_hosts_metadata_definitely_does_not_exist.toml");
    let _ = std::fs::remove_file(&path);
    let result = load_from_path(&path);
    assert!(result.is_empty());
}

#[test]
fn load_from_malformed_toml_returns_empty_map_silently() {
    // Garbage in the file shouldn't crash startup — the model logs and
    // moves on, the user fixes it or the next mutation overwrites.
    let path = std::env::temp_dir().join("ssh_hosts_metadata_malformed_test.toml");
    std::fs::write(&path, "this is not [[ valid toml").expect("write tempfile");
    let result = load_from_path(&path);
    assert!(result.is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_from_valid_file_recovers_expected_metadata() {
    let path = std::env::temp_dir().join("ssh_hosts_metadata_valid_test.toml");
    std::fs::write(
        &path,
        r##"
[hosts.prod-web]
display_name = "Prod webserver"
notes = "Use --max-time 30"
tags = ["prod", "us-east"]
color = "#ff5733"

[hosts.staging]
tags = ["staging"]
"##,
    )
    .expect("write tempfile");

    let result = load_from_path(&path);

    assert_eq!(
        result.get("prod-web"),
        Some(&HostMetadata {
            display_name: Some("Prod webserver".to_string()),
            notes: Some("Use --max-time 30".to_string()),
            tags: vec!["prod".to_string(), "us-east".to_string()],
            color: Some("#ff5733".to_string()),
        })
    );
    assert_eq!(
        result.get("staging"),
        Some(&HostMetadata {
            tags: vec!["staging".to_string()],
            ..Default::default()
        })
    );
    assert_eq!(result.len(), 2);

    let _ = std::fs::remove_file(&path);
}

// ── merge_imported (Phase 1.6) ──────────────────────────────────────

fn metadata_with_display(name: &str) -> HostMetadata {
    HostMetadata {
        display_name: Some(name.to_string()),
        ..Default::default()
    }
}

#[test]
fn merge_into_empty_current_classifies_every_record_as_new() {
    let mut current = HashMap::new();
    let mut imported = HashMap::new();
    imported.insert("prod-web".to_string(), metadata_with_display("Prod web"));
    imported.insert("staging".to_string(), metadata_with_display("Staging"));

    let result = merge_imported(&mut current, imported);

    assert_eq!(result.imported_new, 2);
    assert_eq!(result.overwritten, 0);
    assert_eq!(result.unchanged, 0);
    assert_eq!(current.len(), 2);
}

#[test]
fn merge_overwrites_when_imported_differs_from_existing() {
    let mut current = HashMap::new();
    current.insert("prod-web".to_string(), metadata_with_display("Old name"));
    let mut imported = HashMap::new();
    imported.insert("prod-web".to_string(), metadata_with_display("New name"));

    let result = merge_imported(&mut current, imported);

    assert_eq!(result.imported_new, 0);
    assert_eq!(result.overwritten, 1);
    assert_eq!(result.unchanged, 0);
    assert_eq!(
        current
            .get("prod-web")
            .and_then(|m| m.display_name.as_deref()),
        Some("New name"),
    );
}

#[test]
fn merge_classifies_matching_records_as_unchanged() {
    let mut current = HashMap::new();
    current.insert("prod-web".to_string(), metadata_with_display("Prod web"));
    let mut imported = HashMap::new();
    imported.insert("prod-web".to_string(), metadata_with_display("Prod web"));

    let result = merge_imported(&mut current, imported);

    assert_eq!(result.imported_new, 0);
    assert_eq!(result.overwritten, 0);
    assert_eq!(result.unchanged, 1);
}

#[test]
fn merge_with_mixed_outcomes_reports_all_counts_correctly() {
    let mut current = HashMap::new();
    current.insert("prod-web".to_string(), metadata_with_display("Prod web"));
    current.insert("staging".to_string(), metadata_with_display("Staging"));

    let mut imported = HashMap::new();
    // staging: same → unchanged
    imported.insert("staging".to_string(), metadata_with_display("Staging"));
    // prod-web: differs → overwritten
    imported.insert("prod-web".to_string(), metadata_with_display("Prod-web v2"));
    // dev: new → imported_new
    imported.insert("dev".to_string(), metadata_with_display("Dev"));

    let result = merge_imported(&mut current, imported);

    assert_eq!(result.imported_new, 1);
    assert_eq!(result.overwritten, 1);
    assert_eq!(result.unchanged, 1);
    assert_eq!(result.total(), 3);
    assert!(result.touched_disk());
    assert_eq!(current.len(), 3);
}

#[test]
fn merge_with_no_changes_does_not_touch_disk() {
    let mut current = HashMap::new();
    current.insert("prod-web".to_string(), metadata_with_display("Prod web"));
    let mut imported = HashMap::new();
    imported.insert("prod-web".to_string(), metadata_with_display("Prod web"));

    let result = merge_imported(&mut current, imported);

    assert!(!result.touched_disk());
}

#[test]
fn merge_preserves_existing_aliases_not_present_in_import() {
    // Import is additive — existing aliases that aren't in the
    // imported file are NOT removed. Removal happens only via the
    // prune-on-config-change subscription.
    let mut current = HashMap::new();
    current.insert("prod-web".to_string(), metadata_with_display("Prod web"));
    current.insert("staging".to_string(), metadata_with_display("Staging"));

    let mut imported = HashMap::new();
    imported.insert("dev".to_string(), metadata_with_display("Dev"));

    let _ = merge_imported(&mut current, imported);

    assert_eq!(current.len(), 3);
    assert!(current.contains_key("prod-web"));
    assert!(current.contains_key("staging"));
    assert!(current.contains_key("dev"));
}

#[test]
fn import_result_total_sums_all_three_counts() {
    let r = ImportResult {
        imported_new: 1,
        overwritten: 2,
        unchanged: 3,
    };
    assert_eq!(r.total(), 6);
}

#[test]
fn import_result_touched_disk_excludes_unchanged() {
    assert!(!ImportResult::default().touched_disk());
    assert!(!ImportResult {
        unchanged: 5,
        ..Default::default()
    }
    .touched_disk());
    assert!(ImportResult {
        imported_new: 1,
        ..Default::default()
    }
    .touched_disk());
    assert!(ImportResult {
        overwritten: 1,
        ..Default::default()
    }
    .touched_disk());
}
