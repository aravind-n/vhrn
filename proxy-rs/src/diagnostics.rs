//! Contract-visible responses and denial records.

use std::io::Write;

use vhrn_policy::Mode;

/// A response independent of an HTTP implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseDescriptor {
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub body: String,
}

/// Describes direct endpoints.
#[must_use]
pub fn direct_response(path: &str, mode: Mode) -> ResponseDescriptor {
    match path {
        "/healthz" => ResponseDescriptor {
            status: 200,
            content_type: None,
            body: "ok\n".to_owned(),
        },
        "/__status" => ResponseDescriptor {
            status: 200,
            content_type: Some("application/json"),
            body: format!("{{\"mode\":\"{}\"}}\n", mode.as_str()),
        },
        _ => ResponseDescriptor {
            status: 404,
            content_type: None,
            body: "404 page not found\n".to_owned(),
        },
    }
}
/// Renders one denial record.
#[must_use]
pub fn render_denial(timestamp: &str, destination: &str) -> String {
    format!("{timestamp}\t{destination}\n")
}
/// Writes one denial record if a log is configured.
///
/// # Errors
///
/// Returns an error when the configured file cannot be opened or written.
pub fn write_denial(path: Option<&str>, timestamp: &str, destination: &str) -> std::io::Result<()> {
    if let Some(path) = path {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(render_denial(timestamp, destination).as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    #[test]
    fn direct_endpoint_corpus() {
        for row in include_str!("../../testdata/proxy-process-cases.tsv")
            .lines()
            .filter(|r| !r.starts_with('#'))
        {
            let fields: Vec<_> = row.split('\t').collect();
            if let ["direct", input, status, _] = fields.as_slice() {
                let response = direct_response(input, Mode::Report);
                assert_eq!(response.status.to_string(), *status);
                match *input {
                    "/healthz" => assert_eq!(
                        response,
                        ResponseDescriptor {
                            status: 200,
                            content_type: None,
                            body: "ok\n".to_owned()
                        }
                    ),
                    "/__status" => assert_eq!(
                        response,
                        ResponseDescriptor {
                            status: 200,
                            content_type: Some("application/json"),
                            body: "{\"mode\":\"report\"}\n".to_owned()
                        }
                    ),
                    _ => assert_eq!(
                        response,
                        ResponseDescriptor {
                            status: 404,
                            content_type: None,
                            body: "404 page not found\n".to_owned()
                        }
                    ),
                }
            }
        }
        assert_eq!(
            direct_response("/__status", Mode::Open).body,
            "{\"mode\":\"open\"}\n"
        );
    }
    #[test]
    fn denial_record_is_literal() {
        assert_eq!(
            render_denial("2026-01-02T03:04:05Z", "denied.example"),
            "2026-01-02T03:04:05Z\tdenied.example\n"
        );
    }
    #[test]
    fn denial_output_appends_exact_bytes() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("deny.log");
        write_denial(
            Some(file.to_str().unwrap()),
            "2026-01-02T03:04:05Z",
            "one.example",
        )
        .unwrap();
        write_denial(
            Some(file.to_str().unwrap()),
            "2026-01-02T03:04:06Z",
            "two.example",
        )
        .unwrap();
        assert_eq!(
            fs::read(&file).unwrap(),
            b"2026-01-02T03:04:05Z\tone.example\n2026-01-02T03:04:06Z\ttwo.example\n"
        );
        write_denial(None, "time", "ignored").unwrap();
    }
}
