//! Registry image references, image pulls, and the content-addressed toolchain build.
//! `VHRN_REGISTRY` overrides the default registry.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::harness::Harness;

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
pub(crate) fn parse_harness_arg(arg: &str) -> (String, String) {
    match arg.split_once('@') {
        Some((name, version)) => {
            let version = if version.is_empty() {
                "latest"
            } else {
                version
            };
            (name.to_string(), version.to_string())
        }
        None => (arg.to_string(), "latest".to_string()),
    }
}

/// The image to run for a harness at an installed version: the bare local image for
/// a make-built install, else the versioned registry ref (the version is the agent's).
/// `registry` is the resolved base (see `registry_base`).
pub(crate) fn harness_image_ref(registry: &str, h: &Harness, version: &str) -> String {
    if version == LOCAL_VERSION {
        h.image.clone()
    } else {
        format!("{registry}/{}:{version}", h.image)
    }
}

/// The egress proxy ref at `tag`: the bare make-built name for a local build, else the
/// versioned registry ref. `tag` comes from `proxy_tag`, not the harness version.
pub(crate) fn proxy_image_ref(registry: &str, tag: &str) -> String {
    if tag == LOCAL_VERSION {
        PROXY_IMAGE_NAME.to_string()
    } else {
        format!("{registry}/{PROXY_IMAGE_NAME}:{tag}")
    }
}

/// The proxy tag for a run. The proxy shares runtime contracts with the CLI (the policy
/// files, the port, the entrypoint), so it rides the CLI binary's own version rather than
/// the harness's agent version: a nightly CLI pairs with the nightly proxy, a vX.Y.Z
/// release with its own tag, and any other version (e.g. a locally-built CLI run against
/// registry images) with the latest proxy. A `--local` harness uses the make-built proxy.
pub(crate) fn proxy_tag(cli_version: &str, harness_version: &str) -> String {
    if harness_version == LOCAL_VERSION {
        return LOCAL_VERSION.to_string();
    }
    if cli_version.contains("-nightly") {
        "nightly".to_string()
    } else if cli_version.starts_with('v') {
        cli_version.to_string()
    } else {
        "latest".to_string()
    }
}

// ---- registry image delivery (pull the release images; delete on uninstall) -----

/// Whether the engine already has `image` locally.
fn image_exists(engine: &str, image: &str) -> bool {
    Command::new(engine)
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Make the harness available at `version` (the agent's tag) and its matching proxy at the
/// CLI's own version (see `proxy_tag`): pull both from the registry, or (for `--local`)
/// verify the make-built images exist. `registry` is the resolved base.
pub(crate) fn provision_images(
    engine: &str,
    registry: &str,
    h: &Harness,
    version: &str,
) -> Result<()> {
    if engine == "container" {
        // Apple engine needs its background service up before any image op.
        let _ = Command::new("container").args(["system", "start"]).status();
    }
    let harness_img = harness_image_ref(registry, h, version);
    let proxy_img = proxy_image_ref(registry, &proxy_tag(crate::cli::version(), version));

    if version == LOCAL_VERSION {
        for img in [harness_img.as_str(), proxy_img.as_str()] {
            if !image_exists(engine, img) {
                bail!("local image {img:?} not found — run `make build` first");
            }
        }
        return Ok(());
    }
    // Pull the proxy first, then the harness; either failure aborts the install.
    for img in [proxy_img.as_str(), harness_img.as_str()] {
        info!("pulling {img}...");
        pull_image(engine, img).with_context(|| format!("pulling {img}"))?;
    }
    Ok(())
}

/// The engine image-pull command. Both Docker and Apple container use `<engine> image
/// pull` — Apple container has no top-level `pull` subcommand.
fn pull_argv(image: &str) -> Vec<String> {
    vec!["image".to_string(), "pull".into(), image.into()]
}

/// Pull an image with the engine, streaming progress to stderr (our stdout stays clean).
fn pull_image(engine: &str, image: &str) -> Result<()> {
    use std::os::fd::AsFd;
    let err_out = Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?);
    let status = Command::new(engine)
        .args(pull_argv(image))
        .stdout(err_out)
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        bail!("{engine} pull failed for {image}");
    }
    Ok(())
}

/// The engine-specific image-delete command: Docker and Apple container differ
/// (`image rm` vs `image delete`), so it is not a bare engine-name swap.
fn remove_image_argv(engine: &str, image: &str) -> Vec<String> {
    let verb = if engine == "docker" { "rm" } else { "delete" };
    vec!["image".to_string(), verb.into(), image.into()]
}

/// Delete an image with the engine, streaming output to stderr.
pub(crate) fn remove_image(engine: &str, image: &str) -> Result<()> {
    use std::os::fd::AsFd;
    let err_out = Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?);
    let status = Command::new(engine)
        .args(remove_image_argv(engine, image))
        .stdout(err_out)
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        bail!("{engine} image delete failed for {image}");
    }
    Ok(())
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

/// The engine's local image ID (a content digest) for `image`, or None if it can't be
/// read. Docker templates it out; Apple `container image inspect` prints JSON we scan.
pub(crate) fn image_id(engine: &str, image: &str) -> Option<String> {
    if engine == "docker" {
        let out = Command::new("docker")
            .args(["image", "inspect", "-f", "{{.Id}}", image])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return (!id.is_empty()).then_some(id);
    }
    let out = Command::new("container")
        .args(["image", "inspect", image])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    first_sha256(&String::from_utf8_lossy(&out.stdout))
}

/// The first `sha256:<hex>` token in engine inspect output (Apple container prints JSON).
fn first_sha256(s: &str) -> Option<String> {
    let start = s.find("sha256:")?;
    let hex: String = s[start + 7..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    (hex.len() >= 12).then(|| format!("sha256:{hex}"))
}

/// The OCI label CI stamps the agent version into; the host reads it back to name a
/// harness image's version without running it.
const VERSION_LABEL: &str = "org.opencontainers.image.version";

/// The agent version in `image`'s version label, or None if unreadable/absent. Docker
/// templates the label out; Apple `container image inspect` prints JSON we scan.
pub(crate) fn image_version_label(engine: &str, image: &str) -> Option<String> {
    if engine == "docker" {
        let out = Command::new("docker")
            .args([
                "image",
                "inspect",
                "-f",
                &format!("{{{{index .Config.Labels \"{VERSION_LABEL}\"}}}}"),
                image,
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // docker prints "<no value>" when the label is absent.
        return (!v.is_empty() && v != "<no value>").then_some(v);
    }
    let out = Command::new("container")
        .args(["image", "inspect", image])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    json_string_value(&String::from_utf8_lossy(&out.stdout), VERSION_LABEL)
}

/// Best-effort: the quoted string value following `"<key>"` in JSON. Engine inspect output
/// isn't parsed structurally (Apple's shape varies), so scan for the key's value.
fn json_string_value(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after = json[json.find(&needle)? + needle.len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let rest = after.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The content-addressed image tag for a tool set atop a base image: `<prefix>-tc-<hash12>`
/// (`prefix` is the clean local image name, e.g. vhrn-claude — not the pulled registry ref,
/// which carries a colon and can't prefix a tag). The hash covers the tools *and* `base_id`
/// (the base image's identity), so a rebuilt harness image gets a fresh tag even at an
/// unchanged ref. Same base + same tools -> same tag, built once.
fn toolchain_tag(prefix: &str, base_id: &str, tools: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(normalize_tools(tools).join("\n").as_bytes());
    let hexed = hex::encode(hasher.finalize());
    format!("{prefix}-tc-{}", &hexed[..12])
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
fn build_image(
    engine: &str,
    image: &str,
    dockerfile: &str,
    context: &str,
    extra: &[String],
) -> Result<()> {
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
    // Fold the base image's identity (its content digest, else the ref itself) into the
    // tag, so a rebuilt harness image forces a rebuild here even at an unchanged tag.
    let base_id = image_id(engine, from_image).unwrap_or_else(|| from_image.to_string());
    let tag = toolchain_tag(tag_base, &base_id, &norm);
    if image_exists(engine, &tag) {
        return Ok(tag);
    }
    let tmp = build_temp_dir()?;
    let dockerfile = tmp.join("Dockerfile");
    std::fs::write(&dockerfile, toolchain_dockerfile(from_image, &norm))?;
    info!("provisioning toolchain ({}) into {tag}...", norm.join(", "));
    let result = build_image(
        engine,
        &tag,
        &dockerfile.to_string_lossy(),
        &tmp.to_string_lossy(),
        &[],
    );
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
        assert_eq!(
            resolve_registry(Some("example.com/team")),
            "example.com/team"
        );
    }

    #[test]
    fn parse_harness_arg_cases() {
        let want = |n: &str, v: &str| (n.to_string(), v.to_string());
        assert_eq!(parse_harness_arg("claude"), want("claude", "latest"));
        assert_eq!(parse_harness_arg("claude@v0.2.0"), want("claude", "v0.2.0"));
        assert_eq!(
            parse_harness_arg("claude@sha-abc123"),
            want("claude", "sha-abc123")
        );
        assert_eq!(parse_harness_arg("claude@"), want("claude", "latest")); // trailing @ is latest
    }

    #[test]
    fn image_refs_format() {
        let h = Harness {
            name: "claude".into(),
            image: "vhrn-claude".into(),
            ..Default::default()
        };
        let reg = "ghcr.io/aravind-n";
        assert_eq!(
            harness_image_ref(reg, &h, "v0.2.0"),
            "ghcr.io/aravind-n/vhrn-claude:v0.2.0"
        );
        assert_eq!(
            proxy_image_ref(reg, "v0.2.0"),
            "ghcr.io/aravind-n/vhrn-proxy:v0.2.0"
        );
        // A local install uses the bare, make-built image names (registry ignored).
        assert_eq!(harness_image_ref(reg, &h, LOCAL_VERSION), "vhrn-claude");
        assert_eq!(proxy_image_ref(reg, LOCAL_VERSION), "vhrn-proxy");
        // An override registry is used verbatim.
        assert_eq!(
            harness_image_ref("example.com/team", &h, "latest"),
            "example.com/team/vhrn-claude:latest"
        );
    }

    #[test]
    fn proxy_tag_rides_cli_version() {
        // A --local harness always uses the make-built proxy, whatever the CLI version.
        assert_eq!(proxy_tag("v0.1.0", LOCAL_VERSION), LOCAL_VERSION);
        // Otherwise the proxy rides the CLI's own version, not the agent's.
        assert_eq!(proxy_tag("v0.1.0", "2.1.30"), "v0.1.0"); // release
        assert_eq!(proxy_tag("v0.2.0", "latest"), "v0.2.0");
        assert_eq!(
            proxy_tag("0.1.0-nightly.20260101.abc", "nightly"),
            "nightly"
        );
        assert_eq!(proxy_tag("0.1.0", "latest"), "latest"); // locally-built CLI
    }

    #[test]
    fn toolchain_tag_stable() {
        let a = toolchain_tag(
            "vhrn-claude",
            "sha256:aa",
            &["go@1.26".into(), "node@22".into()],
        );
        // reorder + whitespace + dup must not change the tag.
        let b = toolchain_tag(
            "vhrn-claude",
            "sha256:aa",
            &["node@22".into(), " go@1.26 ".into(), "node@22".into()],
        );
        assert_eq!(a, b, "tag must be order/whitespace/dup independent");
        assert!(a.starts_with("vhrn-claude-tc-"), "unexpected tag {a}");
        assert_ne!(
            toolchain_tag("vhrn-claude", "sha256:aa", &["go@1.26".into()]),
            a,
            "different tool sets should differ"
        );
        // A changed base image identity (a rebuilt harness) must change the tag.
        assert_ne!(
            toolchain_tag(
                "vhrn-claude",
                "sha256:bb",
                &["go@1.26".into(), "node@22".into()]
            ),
            a,
            "a new base image must force a new toolchain tag"
        );
    }

    #[test]
    fn first_sha256_extracts_digest() {
        assert_eq!(
            first_sha256(r#"{"Id":"sha256:abcdef0123456789"}"#),
            Some("sha256:abcdef0123456789".to_string())
        );
        assert_eq!(first_sha256("no digest here"), None);
        assert_eq!(first_sha256("sha256:abc"), None); // fewer than 12 hex chars
    }

    #[test]
    fn json_string_value_scans_key() {
        let j = r#"{"Config":{"Labels":{"org.opencontainers.image.version":"2.1.31"}}}"#;
        assert_eq!(
            json_string_value(j, "org.opencontainers.image.version"),
            Some("2.1.31".to_string())
        );
        assert_eq!(json_string_value(j, "missing.key"), None);
        // spacing around the colon is tolerated
        assert_eq!(
            json_string_value(r#""k" : "v""#, "k"),
            Some("v".to_string())
        );
    }

    #[test]
    fn toolchain_dockerfile_contents() {
        let df = toolchain_dockerfile("vhrn-claude", &["node@22".into(), "go@1.26".into()]);
        assert!(df.contains("FROM vhrn-claude"), "missing FROM:\n{df}");
        assert!(
            df.contains("mise use -g go@1.26 node@22"),
            "tools not in sorted order:\n{df}"
        );
        assert!(
            df.contains("USER dev") && df.contains("USER root"),
            "provision as dev then root:\n{df}"
        );
    }

    #[test]
    fn ensure_toolchain_image_no_tools_passes_through() {
        // No tools must pass the harness image through untouched, without touching the engine.
        let img =
            ensure_toolchain_image("container", "ghcr.io/x/vhrn-claude:v1", "vhrn-claude", &[])
                .unwrap();
        assert_eq!(img, "ghcr.io/x/vhrn-claude:v1");
    }

    #[test]
    fn build_argv_layout() {
        assert_eq!(
            build_argv(
                "img:tag",
                "/ctx/Dockerfile",
                "/ctx",
                &["--build-arg".into(), "K=V".into()]
            ),
            [
                "build",
                "--tag",
                "img:tag",
                "--file",
                "/ctx/Dockerfile",
                "--build-arg",
                "K=V",
                "/ctx"
            ]
        );
    }

    #[test]
    fn pull_argv_layout() {
        // Both engines pull via `<engine> image pull` — Apple container has no
        // top-level `pull` subcommand.
        assert_eq!(
            pull_argv("ghcr.io/aravind-n/vhrn-claude:v0.2.0"),
            ["image", "pull", "ghcr.io/aravind-n/vhrn-claude:v0.2.0"]
        );
    }

    #[test]
    fn remove_image_argv_per_engine() {
        // Docker deletes with `image rm`; Apple container with `image delete`.
        assert_eq!(
            remove_image_argv("docker", "vhrn-claude"),
            ["image", "rm", "vhrn-claude"]
        );
        assert_eq!(
            remove_image_argv("container", "vhrn-claude"),
            ["image", "delete", "vhrn-claude"]
        );
    }
}
