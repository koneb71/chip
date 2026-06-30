//! Input validation for identifiers that become storage paths / S3 keys.

/// Whether `name` is safe to use as a username or repository name.
///
/// Allows 1..=64 characters, each ASCII alphanumeric, `-`, or `_`. This rejects
/// `.`, `/`, and therefore `..` and any path-traversal attempt, since those
/// characters are not in the allowed set.
pub fn valid_name(name: &str) -> bool {
    let len = name.len();
    (1..=64).contains(&len)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn accepts_reasonable_names() {
        for n in ["alice", "my-repo", "repo_2", "ABC123"] {
            assert!(valid_name(n), "{n} should be valid");
        }
    }

    #[test]
    fn rejects_traversal_and_junk() {
        for n in [
            "",
            "..",
            ".",
            "a/b",
            "../x",
            "a.b",
            "with space",
            "tab\t",
            &"x".repeat(65),
        ] {
            assert!(!valid_name(n), "{n:?} should be invalid");
        }
    }
}
