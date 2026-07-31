/// Return true if SUPVAN_MOCK=1.
pub fn is_mock_mode() -> bool {
    std::env::var("SUPVAN_MOCK")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Slugify a printer-reported name into something CUPS can use as a queue
/// name. Lowercase ASCII alphanumerics; everything else becomes a hyphen.
///
/// The result is half of a printer's identity: `list` builds its
/// `supvan://<slug>` device URI from it, and that URI is what the framework
/// persists and matches an already-configured printer on. Anything deciding
/// whether a discovered candidate is already known has to slug its name the
/// same way.
pub fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s: String = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if s.is_empty() {
        "printer".to_string()
    } else {
        s
    }
}
