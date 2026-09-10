//! Checks a measured candidate against fixed proxy resource thresholds.

use serde::Deserialize;

const MIB: u64 = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    reference: Metrics,
    candidate: Metrics,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metrics {
    startup_ms: f64,
    idle_rss_bytes: u64,
    throughput: f64,
    p95_latency_ms: f64,
    post_stress_rss_bytes: u64,
    retained_tasks: u64,
    retained_sockets: u64,
}

fn main() -> Result<(), String> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let [path] = arguments.as_slice() else {
        return Err("usage: resource_compare <evidence.json>".to_owned());
    };
    let input = std::fs::read_to_string(path).map_err(|error| format!("read {path}: {error}"))?;
    let evidence: Evidence =
        serde_json::from_str(&input).map_err(|error| format!("invalid evidence: {error}"))?;
    validate(&evidence)?;
    println!("resource comparison passed");
    Ok(())
}

fn validate(evidence: &Evidence) -> Result<(), String> {
    validate_metrics("reference", &evidence.reference)?;
    validate_metrics("candidate", &evidence.candidate)?;
    let reference = &evidence.reference;
    let candidate = &evidence.candidate;
    check_max_f64(
        "startup_ms",
        candidate.startup_ms,
        (2.0 * reference.startup_ms).max(500.0),
    )?;
    check_max_u64(
        "idle_rss_bytes",
        candidate.idle_rss_bytes,
        reference.idle_rss_bytes.saturating_mul(2).max(32 * MIB),
    )?;
    check_min(
        "throughput",
        candidate.throughput,
        0.7 * reference.throughput,
    )?;
    check_max_f64(
        "p95_latency_ms",
        candidate.p95_latency_ms,
        2.0 * reference.p95_latency_ms,
    )?;
    check_max_u64(
        "post_stress_rss_bytes",
        candidate.post_stress_rss_bytes,
        candidate
            .idle_rss_bytes
            .saturating_add((candidate.idle_rss_bytes / 10).max(2 * MIB)),
    )?;
    check_max_u64("retained_tasks", candidate.retained_tasks, 0)?;
    check_max_u64("retained_sockets", candidate.retained_sockets, 0)
}

fn validate_metrics(name: &str, metrics: &Metrics) -> Result<(), String> {
    for (field, value) in [
        ("startup_ms", metrics.startup_ms),
        ("throughput", metrics.throughput),
        ("p95_latency_ms", metrics.p95_latency_ms),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(format!("{name}.{field} must be finite and nonnegative"));
        }
    }
    Ok(())
}

fn check_max_f64(field: &str, candidate: f64, limit: f64) -> Result<(), String> {
    if candidate > limit {
        return Err(format!(
            "{field} failed: candidate={candidate}, limit={limit}"
        ));
    }
    Ok(())
}

fn check_max_u64(field: &str, candidate: u64, limit: u64) -> Result<(), String> {
    if candidate > limit {
        return Err(format!(
            "{field} failed: candidate={candidate}, limit={limit}"
        ));
    }
    Ok(())
}

fn check_min(field: &str, candidate: f64, limit: f64) -> Result<(), String> {
    if candidate < limit {
        return Err(format!(
            "{field} failed: candidate={candidate}, limit={limit}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> Evidence {
        Evidence {
            reference: Metrics {
                startup_ms: 250.0,
                idle_rss_bytes: 16 * MIB,
                throughput: 100.0,
                p95_latency_ms: 8.0,
                post_stress_rss_bytes: 16 * MIB,
                retained_tasks: 1,
                retained_sockets: 1,
            },
            candidate: Metrics {
                startup_ms: 500.0,
                idle_rss_bytes: 32 * MIB,
                throughput: 70.0,
                p95_latency_ms: 16.0,
                post_stress_rss_bytes: 32 * MIB + (32 * MIB / 10),
                retained_tasks: 0,
                retained_sockets: 0,
            },
        }
    }

    #[test]
    fn accepts_every_boundary() {
        assert!(validate(&evidence()).is_ok());
    }

    #[test]
    fn rejects_each_threshold_independently() {
        let cases: [(&str, fn(&mut Metrics)); 7] = [
            ("startup_ms", |metrics: &mut Metrics| {
                metrics.startup_ms = 500.1
            }),
            ("idle_rss_bytes", |metrics: &mut Metrics| {
                metrics.idle_rss_bytes = 32 * MIB + 1
            }),
            ("throughput", |metrics: &mut Metrics| {
                metrics.throughput = 69.9
            }),
            ("p95_latency_ms", |metrics: &mut Metrics| {
                metrics.p95_latency_ms = 16.1
            }),
            ("post_stress_rss_bytes", |metrics: &mut Metrics| {
                metrics.post_stress_rss_bytes = 32 * MIB + (32 * MIB / 10) + 1
            }),
            ("retained_tasks", |metrics: &mut Metrics| {
                metrics.retained_tasks = 1
            }),
            ("retained_sockets", |metrics: &mut Metrics| {
                metrics.retained_sockets = 1
            }),
        ];
        for (field, update) in cases {
            let mut value = evidence();
            update(&mut value.candidate);
            assert!(validate(&value).unwrap_err().starts_with(field), "{field}");
        }
    }

    #[test]
    fn rejects_malformed_schema_and_numbers() {
        for input in [
            "{}",
            r#"{"reference":{},"candidate":{}}"#,
            r#"{"reference":{"startup_ms":0,"idle_rss_bytes":0,"throughput":0,"p95_latency_ms":0,"post_stress_rss_bytes":0,"retained_tasks":0,"retained_sockets":0,"extra":1},"candidate":{"startup_ms":0,"idle_rss_bytes":0,"throughput":0,"p95_latency_ms":0,"post_stress_rss_bytes":0,"retained_tasks":0,"retained_sockets":0}}"#,
            r#"{"reference":{"startup_ms":-1,"idle_rss_bytes":0,"throughput":0,"p95_latency_ms":0,"post_stress_rss_bytes":0,"retained_tasks":0,"retained_sockets":0},"candidate":{"startup_ms":0,"idle_rss_bytes":0,"throughput":0,"p95_latency_ms":0,"post_stress_rss_bytes":0,"retained_tasks":0,"retained_sockets":0}}"#,
        ] {
            let parsed = serde_json::from_str::<Evidence>(input);
            assert!(parsed.as_ref().is_err() || validate(parsed.as_ref().unwrap()).is_err());
        }
        let mut nonfinite = evidence();
        nonfinite.candidate.throughput = f64::NAN;
        assert!(validate(&nonfinite).is_err());
    }
}
