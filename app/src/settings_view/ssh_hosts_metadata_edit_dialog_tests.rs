use super::{parse_tag_list, trimmed_or_none};

#[test]
fn trimmed_or_none_returns_none_for_empty() {
    assert_eq!(trimmed_or_none(""), None);
    assert_eq!(trimmed_or_none("   "), None);
    assert_eq!(trimmed_or_none("\t\n"), None);
}

#[test]
fn trimmed_or_none_returns_trimmed_value() {
    assert_eq!(trimmed_or_none("  hello  ").as_deref(), Some("hello"));
    assert_eq!(trimmed_or_none("hello").as_deref(), Some("hello"));
}

#[test]
fn parse_tag_list_splits_on_commas() {
    let tags = parse_tag_list("prod, us-east, web");
    assert_eq!(tags, vec!["prod", "us-east", "web"]);
}

#[test]
fn parse_tag_list_drops_empty_entries() {
    let tags = parse_tag_list(",prod,,us-east,");
    assert_eq!(tags, vec!["prod", "us-east"]);
}

#[test]
fn parse_tag_list_trims_whitespace_around_entries() {
    let tags = parse_tag_list("  prod  ,   us-east");
    assert_eq!(tags, vec!["prod", "us-east"]);
}

#[test]
fn parse_tag_list_empty_string_returns_empty_vec() {
    let tags = parse_tag_list("");
    assert!(tags.is_empty());
}

#[test]
fn parse_tag_list_whitespace_only_returns_empty_vec() {
    let tags = parse_tag_list("   ,  ,   ");
    assert!(tags.is_empty());
}

#[test]
fn parse_tag_list_preserves_order_and_duplicates() {
    // We don't dedupe at parse time — the model layer can choose to
    // later, but the user's typing order is preserved here.
    let tags = parse_tag_list("a, b, a, c, b");
    assert_eq!(tags, vec!["a", "b", "a", "c", "b"]);
}
