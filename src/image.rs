//! Registry image references and the toolchain content-hash helpers. `VHRN_REGISTRY`
//! overrides the default registry. Ports the pure parts of images.go + toolchain.go;
//! the image pull and the derived toolchain build land in later phases.

use crate::harness::Harness;
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const BASE_IMAGE_NAME: &str = "vhrn-base";
const PROXY_IMAGE_NAME: &str = "vhrn-proxy";
const DEFAULT_REGISTRY: &str = "ghcr.io/aravind-n";
/// Marks a make-built image used as-is (bare name, no registry) rather than one
/// pulled from the registry.
pub(crate) const LOCAL_VERSION: &str = "local";

/// Pick the registry base from an injected env value: `VHRN_REGISTRY` when set and
/// non-empty, else the default. Split from the read so it is unit-testable without
/// touching (or mutating) process env.
fn resolve_registry(env: Option<&str>) -> String {
    match env {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => DEFAULT_REGISTRY.to_string(),
    }
}

/// The registry base, reading `VHRN_REGISTRY` at the edge.
pub(crate) fn registry_base() -> String {
    resolve_registry(std::env::var("VHRN_REGISTRY").ok().as_deref())
}

/// Split "claude" or "claude@v0.2.0" into name and version, defaulting to "latest"
/// when no @tag (or a bare trailing @) is given.
fn parse_harness_arg(arg: &str) -> (String, String) {
    match arg.split_once('@') {
        Some((name, version)) => {
            let version = if version.is_empty() { "latest" } else { version };
            (name.to_string(), version.to_string())
        }
        None => (arg.to_string(), "latest".to_string()),
    }
}

/// The image to run for a harness at an installed version: the bare local image for
/// a make-built install, else the versioned registry ref. `registry` is the resolved
/// base (see registry_base). The proxy is pinned to the same version, so a box and
/// its proxy are always a set.
pub(crate) fn harness_image_ref(registry: &str, h: &Harness, version: &str) -> String {
    if version == LOCAL_VERSION {
        h.image.clone()
    } else {
        format!("{registry}/{}:{version}", h.image)
    }
}

/// The egress proxy ref, pinned to the same version as the harness it serves.
pub(crate) fn proxy_image_ref(registry: &str, version: &str) -> String {
    if version == LOCAL_VERSION {
        PROXY_IMAGE_NAME.to_string()
    } else {
        format!("{registry}/{PROXY_IMAGE_NAME}:{version}")
    }
}

/// Trim, drop empties, de-duplicate, and sort a tool list so the content hash is
/// stable regardless of order or incidental whitespace.
fn normalize_tools(tools: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for t in tools {
        let t = t.trim();
        if t.is_empty() || !seen.insert(t.to_string()) {
            continue;
        }
        out.push(t.to_string());
    }
    out.sort();
    out
}

/// The content-addressed image tag for a tool set: `<base>-tc-<hash12>` (base is the
/// clean local image name, e.g. vhrn-claude — not the pulled registry ref, which
/// carries a colon and can't be a tag prefix). Same tools -> same tag, built once.
fn toolchain_tag(base: &str, tools: &[String]) -> String {
    let sum = Sha256::digest(normalize_tools(tools).join("\n").as_bytes());
    let hexed = hex::encode(sum);
    format!("{base}-tc-{}", &hexed[..12])
}

/// A Dockerfile deriving an image FROM the harness image that provisions the tools
/// with mise, as the unprivileged dev user (mise installs into its home).
fn toolchain_dockerfile(base_image: &str, tools: &[String]) -> String {
    format!(
        "FROM {base_image}\nUSER dev\nRUN mise use -g {}\nUSER root\n",
        normalize_tools(tools).join(" ")
    )
}

// ---- toolchain local build (only the derived toolchain image is built locally;
// user-facing images are pulled) --------------------------------------------------

/// Whether the engine already has `image` locally.
fn image_exists(engine: &str, image: &str) -> bool {
    Command::new(engine)
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The engine build command line (pure, for testing).
fn build_argv(image: &str, dockerfile: &str, context: &str, extra: &[String]) -> Vec<String> {
    let mut args = vec![
        "build".to_string(),
        "--tag".into(),
        image.into(),
        "--file".into(),
        dockerfile.into(),
    ];
    args.extend(extra.iter().cloned());
    args.push(context.into());
    args
}

/// A build-context temp dir under the vhrn cache. It must live in the home tree, not
/// the system temp: Apple container's build cannot read a context under macOS's
/// /var/folders and silently drops files from it (invariant #13).
fn build_temp_dir() -> Result<PathBuf> {
    let home = crate::run::home_dir()?;
    let root = crate::run::vhrn_cache(&home).join("build");
    std::fs::create_dir_all(&root)?;
    let dir = root.join(format!("ctx-{}-{}", std::process::id(), next_ctx_id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn next_ctx_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    CTR.fetch_add(1, Ordering::Relaxed)
}

/// Run the engine build, streaming output so the user sees progress. Build chatter
/// goes to our stderr (both streams), keeping vhrn's stdout clean.
fn build_image(engine: &str, image: &str, dockerfile: &str, context: &str, extra: &[String]) -> Result<()> {
    use std::os::fd::AsFd;
    let err_out = Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?);
    let status = Command::new(engine)
        .args(build_argv(image, dockerfile, context, extra))
        .stdout(err_out)
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        bail!("{engine} build failed for {image}");
    }
    Ok(())
}

/// The image to run: `from_image` unchanged when no tools are declared, else a
/// content-addressed derived image (FROM `from_image`, tagged from the clean
/// `tag_base`), built once and cached by its tag. `from_image` is the pulled ref (the
/// FROM); `tag_base` is the clean image name — a ref with a colon can't prefix a tag.
pub(crate) fn ensure_toolchain_image(
    engine: &str,
    from_image: &str,
    tag_base: &str,
    tools: &[String],
) -> Result<String> {
    let norm = normalize_tools(tools);
    if norm.is_empty() {
        return Ok(from_image.to_string());
    }
    let tag = toolchain_tag(tag_base, &norm);
    if image_exists(engine, &tag) {
        return Ok(tag);
    }
    let tmp = build_temp_dir()?;
    let dockerfile = tmp.join("Dockerfile");
    std::fs::write(&dockerfile, toolchain_dockerfile(from_image, &norm))?;
    eprintln!("vhrn: provisioning toolchain ({}) into {tag}...", norm.join(", "));
    let result = build_image(engine, &tag, &dockerfile.to_string_lossy(), &tmp.to_string_lossy(), &[]);
    let _ = std::fs::remove_dir_all(&tmp);
    result?;
    Ok(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_registry_default_and_override() {
        assert_eq!(resolve_registry(None), "ghcr.io/aravind-n");
        assert_eq!(resolve_registry(Some("")), "ghcr.io/aravind-n"); // empty == unset
        assert_eq!(resolve_registry(Some("example.com/team")), "example.com/team");
    }

    #[test]
    fn parse_harness_arg_cases() {
        let want = |n: &str, v: &str| (n.to_string(), v.to_string());
        assert_eq!(parse_harness_arg("claude"), want("claude", "latest"));
        assert_eq!(parse_harness_arg("claude@v0.2.0"), want("claude", "v0.2.0"));
        assert_eq!(parse_harness_arg("claude@sha-abc123"), want("claude", "sha-abc123"));
        assert_eq!(parse_harness_arg("claude@"), want("claude", "latest")); // trailing @ is latest
    }

    #[test]
    fn image_refs_format() {
        let h = Harness { name: "claude".into(), image: "vhrn-claude".into(), ..Default::default() };
        let reg = "ghcr.io/aravind-n";
        assert_eq!(harness_image_ref(reg, &h, "v0.2.0"), "ghcr.io/aravind-n/vhrn-claude:v0.2.0");
        assert_eq!(proxy_image_ref(reg, "v0.2.0"), "ghcr.io/aravind-n/vhrn-proxy:v0.2.0");
        // A local install uses the bare, make-built image names (registry ignored).
        assert_eq!(harness_image_ref(reg, &h, LOCAL_VERSION), "vhrn-claude");
        assert_eq!(proxy_image_ref(reg, LOCAL_VERSION), "vhrn-proxy");
        // An override registry is used verbatim.
        assert_eq!(harness_image_ref("example.com/team", &h, "latest"), "example.com/team/vhrn-claude:latest");
    }

    #[test]
    fn toolchain_tag_stable() {
        let a = toolchain_tag("vhrn-claude", &["go@1.26".into(), "node@22".into()]);
        // reorder + whitespace + dup must not change the tag.
        let b = toolchain_tag("vhrn-claude", &["node@22".into(), " go@1.26 ".into(), "node@22".into()]);
        assert_eq!(a, b, "tag must be order/whitespace/dup independent");
        assert!(a.starts_with("vhrn-claude-tc-"), "unexpected tag {a}");
        assert_ne!(toolchain_tag("vhrn-claude", &["go@1.26".into()]), a, "different tool sets should differ");
    }

    #[test]
    fn toolchain_dockerfile_contents() {
        let df = toolchain_dockerfile("vhrn-claude", &["node@22".into(), "go@1.26".into()]);
        assert!(df.contains("FROM vhrn-claude"), "missing FROM:\n{df}");
        assert!(df.contains("mise use -g go@1.26 node@22"), "tools not in sorted order:\n{df}");
        assert!(df.contains("USER dev") && df.contains("USER root"), "provision as dev then root:\n{df}");
    }

    #[test]
    fn ensure_toolchain_image_no_tools_passes_through() {
        // No tools must pass the harness image through untouched, without touching the engine.
        let img = ensure_toolchain_image("container", "ghcr.io/x/vhrn-claude:v1", "vhrn-claude", &[]).unwrap();
        assert_eq!(img, "ghcr.io/x/vhrn-claude:v1");
    }

    #[test]
    fn build_argv_layout() {
        assert_eq!(
            build_argv("img:tag", "/ctx/Dockerfile", "/ctx", &["--build-arg".into(), "K=V".into()]),
            ["build", "--tag", "img:tag", "--file", "/ctx/Dockerfile", "--build-arg", "K=V", "/ctx"]
        );
    }
}
