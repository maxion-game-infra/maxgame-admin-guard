//! `sanitize_target` (CONTRACT.md §9): what may and may not appear in the
//! `target` field of a check.

use maxgame_readiness::sanitize_target;

#[test]
fn strips_userinfo_query_and_fragment() {
    assert_eq!(
        sanitize_target("https://user:pw@host:8443/a/b?k=1#f"),
        Some("https://host:8443/a/b".to_string())
    );
}

#[test]
fn keeps_scheme_host_port_and_path() {
    assert_eq!(sanitize_target("http://h"), Some("http://h".to_string()));
    assert_eq!(
        sanitize_target("redis://127.0.0.1:6379"),
        Some("redis://127.0.0.1:6379".to_string())
    );
    assert_eq!(
        sanitize_target("postgres://user:pass@db.internal:5432/app?sslmode=require"),
        Some("postgres://db.internal:5432/app".to_string())
    );
    assert_eq!(
        sanitize_target("http://[::1]:8545"),
        Some("http://[::1]:8545".to_string())
    );
}

#[test]
fn rejects_what_is_not_a_url() {
    assert_eq!(sanitize_target("not a url"), None);
    assert_eq!(sanitize_target(""), None);
    assert_eq!(sanitize_target("db.internal:5432"), None, "no scheme");
    assert_eq!(sanitize_target("mailto:someone@example.com"), None, "no //");
    assert_eq!(
        sanitize_target("http://[::1"),
        None,
        "unterminated IPv6 literal"
    );
}
