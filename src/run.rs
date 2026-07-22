//! The run path — box preparation, engine selection, and the proxy sidecar. Ports
//! the pure history-key encoding now; the rest arrives with the engine in a later
//! phase.

/// Reproduce Claude's `projects/<key>` encoding so in-box history unifies with
/// native history: every character outside `[A-Za-z0-9]` becomes `-`
/// (sed 's/[^A-Za-z0-9]/-/g').
fn history_key(project: &str) -> String {
    project
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_key_encoding() {
        let cases = [
            ("/Users/aravind/projects/vhrn", "-Users-aravind-projects-vhrn"),
            ("/a/b_c.d", "-a-b-c-d"),
            ("/x/y-z", "-x-y-z"),
        ];
        for (input, want) in cases {
            assert_eq!(history_key(input), want, "history_key({input:?})");
        }
    }
}
