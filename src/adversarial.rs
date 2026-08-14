//! Adversarial inputs for every parser that consumes bytes senv did not write.
//!
//! `senv.toml`, `pyproject.toml`, directory names, and the output of any
//! command senv runs are all chosen by code the boundary exists to contain. A
//! panic in any of the functions below is a denial of service on the security
//! tool itself — and a panic partway through a state update is worse than that,
//! because the next command reads what it left behind.
//!
//! This is a cheap standing sweep, not a substitute for the targeted tests
//! beside each parser. It exists so that a new branch in one of them is
//! exercised against hostile shapes without anyone having to remember to.

// Tests assert; see the note on the test modules in `src/`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects
)]

/// Every parser here consumes bytes chosen by code the boundary contains.
/// A panic is a denial of service on the security tool, and in the middle
/// of a state update it is worse than that.
fn nasty() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for base in [
        "",
        " ",
        "\0",
        "\u{1b}[2K",
        "\r\n",
        "\u{202e}",
        "\u{feff}",
        "..",
        "../..",
        "/",
        "//",
        "/.",
        "~",
        "~~",
        "~/",
        "-",
        "--",
        ":",
        "::",
        "[",
        "]",
        "[]",
        "[::1]",
        "[::1]:x",
        "a:",
        ":80",
        "\u{10ffff}",
        "é",
        "🙂",
        "\t",
        "'",
        "\"",
        "\\",
        "$(id)",
        "`id`",
        "%s%s%s",
        "{}",
        "{0}",
        "a.b.",
        ".a",
        "*.",
        "*",
        "*.*",
    ] {
        v.push(base.to_string());
        v.push(base.repeat(64));
        v.push(format!("{base}{}", "A".repeat(300)));
        v.push(format!("/{}/{base}", "x".repeat(200)));
    }
    v.push("\u{1b}]8;;".to_string());
    v.push("\u{1b}[".to_string());
    v
}

#[test]
fn no_untrusted_input_parser_panics() {
    let r = crate::receipt::Receipts::new(
        std::path::PathBuf::from("/dev/null"),
        vec![std::path::PathBuf::from("/state/venv")],
    );
    for s in nasty() {
        // Denial inference, over lines a program chose to print.
        let _ = r.analyze(&s);
        let _ = r.analyze(&format!("Permission denied: '{s}'"));
        let _ = r.analyze(&format!("Failed to resolve '{s}' (getaddrinfo)"));
        let _ = r.analyze(&format!("{s}\nOperation not permitted\nsocket.py"));
        // Config-level validators, over senv.toml text.
        let _ = crate::config::validate_host_pattern(&s);
        let _ = crate::config::validate_python_request(&s);
        // Text senv quotes back inside its own messages.
        let _ = crate::util::sanitize(&s);
        let _ = crate::util::sanitize_multiline(&s);
        let _ = crate::util::tail(&s, 7);
        let _ = crate::util::expand_tilde(&s);
        let _ = crate::exec::wrap(&s, 20, 3);
        // Manifest shapes.
        let _ = crate::uv::staging_for(&s);
        // Project keys, from directory names.
        let _ = crate::project::project_key(std::path::Path::new(&s));
        // Whole-config parse.
        let _ = toml::from_str::<crate::config::Config>(&s);
    }
}

#[test]
fn a_project_key_is_always_usable_however_odd_the_directory_name() {
    for s in nasty() {
        let key = crate::project::project_key(std::path::Path::new(&s));
        assert!(!key.is_empty(), "empty key for {s:?}");
        assert!(
            key.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "unusable key {key:?} for {s:?}"
        );
        assert!(key.len() <= 64, "key too long ({}) for {s:?}", key.len());
    }
}
