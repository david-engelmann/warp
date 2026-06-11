use warp_ssh_config::HostDetail;

use super::format_target;

fn host(alias: &str, hostname: Option<&str>, user: Option<&str>, port: Option<u16>) -> HostDetail {
    HostDetail {
        alias: alias.to_string(),
        hostname: hostname.map(str::to_string),
        user: user.map(str::to_string),
        port,
        identity_files: Vec::new(),
        proxy_jump: None,
    }
}

#[test]
fn full_target_renders_user_at_host_colon_port() {
    let h = host(
        "prod-web",
        Some("web.example.com"),
        Some("deploy"),
        Some(2222),
    );
    assert_eq!(
        format_target(&h).as_deref(),
        Some("deploy@web.example.com:2222")
    );
}

#[test]
fn hostname_omitted_falls_back_to_alias() {
    // `HostName` not set in config → ssh would dial the literal alias.
    let h = host("staging", None, Some("root"), None);
    assert_eq!(format_target(&h).as_deref(), Some("root@staging"));
}

#[test]
fn user_omitted_skips_at_sign() {
    let h = host("bastion", Some("10.0.0.1"), None, None);
    assert_eq!(format_target(&h).as_deref(), Some("10.0.0.1"));
}

#[test]
fn port_omitted_skips_colon_port() {
    let h = host("prod-web", Some("web.example.com"), Some("deploy"), None);
    assert_eq!(format_target(&h).as_deref(), Some("deploy@web.example.com"));
}

#[test]
fn minimal_host_with_nothing_specified_still_returns_alias() {
    // Only the alias is present; we still produce a usable target so the
    // subtitle line isn't empty.
    let h = host("just-alias", None, None, None);
    assert_eq!(format_target(&h).as_deref(), Some("just-alias"));
}

#[test]
fn port_is_appended_when_user_is_absent() {
    let h = host("prod-web", Some("web.example.com"), None, Some(2222));
    assert_eq!(format_target(&h).as_deref(), Some("web.example.com:2222"));
}
