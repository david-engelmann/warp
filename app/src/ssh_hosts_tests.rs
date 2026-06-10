use std::path::Path;

use super::is_ssh_config_path;

#[test]
fn matches_canonical_config_file() {
    let ssh = Path::new("/home/user/.ssh");
    assert!(is_ssh_config_path(&ssh.join("config"), ssh,));
}

#[test]
fn matches_conf_suffix() {
    let ssh = Path::new("/home/user/.ssh");
    assert!(is_ssh_config_path(&ssh.join("work.conf"), ssh,));
}

#[test]
fn matches_dot_config_suffix() {
    let ssh = Path::new("/home/user/.ssh");
    assert!(is_ssh_config_path(&ssh.join("aws.config"), ssh,));
}

#[test]
fn matches_any_file_in_config_d_subdir() {
    let ssh = Path::new("/home/user/.ssh");
    assert!(is_ssh_config_path(&ssh.join("config.d").join("work"), ssh,));
}

#[test]
fn matches_any_file_in_conf_d_subdir() {
    let ssh = Path::new("/home/user/.ssh");
    assert!(is_ssh_config_path(&ssh.join("conf.d").join("staging"), ssh,));
}

#[test]
fn rejects_path_outside_ssh_dir() {
    let ssh = Path::new("/home/user/.ssh");
    assert!(!is_ssh_config_path(
        Path::new("/home/user/other/config"),
        ssh,
    ));
}

#[test]
fn rejects_unrelated_file_in_ssh_dir() {
    let ssh = Path::new("/home/user/.ssh");
    // id_rsa, known_hosts, etc. are not config files even though they
    // live in `~/.ssh/`.
    assert!(!is_ssh_config_path(&ssh.join("id_rsa"), ssh,));
    assert!(!is_ssh_config_path(&ssh.join("known_hosts"), ssh,));
    assert!(!is_ssh_config_path(&ssh.join("authorized_keys"), ssh,));
}

#[test]
fn rejects_directory_without_filename() {
    let ssh = Path::new("/home/user/.ssh");
    // A path with no `file_name` (e.g. `/`) should be rejected
    // rather than panicking.
    assert!(!is_ssh_config_path(Path::new("/"), ssh));
}
