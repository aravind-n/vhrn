//! Registry image references, image pulls, and the content-addressed tools build.
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

/// Trim, drop empties, de-duplicate, and sort apt packages: apt install order is
/// irrelevant, so normalizing keeps the content hash stable regardless of listing order
/// or incidental whitespace.
fn normalize_apt(pkgs: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in pkgs {
        let p = p.trim();
        if p.is_empty() || !seen.insert(p.to_string()) {
            continue;
        }
        out.push(p.to_string());
    }
    out.sort();
    out
}

/// Trim and drop empty `run` lines, but PRESERVE order and duplicates: a run sequence is
/// ordered (a later command can depend on an earlier one), so it must never be sorted or
/// de-duplicated the way apt packages are.
fn normalize_run(run: &[String]) -> Vec<String> {
    run.iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The canonical identity of a tools layer. Apt is a set while run is an ordered program;
/// keep that distinction in one type so profile planning, hashing, and building agree.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NormalizedTools {
    pub(crate) apt: Vec<String>,
    pub(crate) run: Vec<String>,
}

impl NormalizedTools {
    pub(crate) fn new(apt: &[String], run: &[String]) -> Self {
        Self {
            apt: normalize_apt(apt),
            run: normalize_run(run),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.apt.is_empty() && self.run.is_empty()
    }
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

/// The registry manifest digest (`sha256:…`) a local image was pulled at — the same digest
/// the tag resolves to in the registry, for the nightly digest check. This is *not* `image_id`:
/// Docker's `.Id` is the config digest and would never match a tag's `Docker-Content-Digest`,
/// so Docker reads `RepoDigests[0]` (`repo@sha256:…`) instead; Apple `container` already
/// exposes the manifest digest as inspect's first `sha256:` (`configuration.descriptor.digest`).
/// None if the image is absent or was never pulled from a registry (empty `RepoDigests`).
pub(crate) fn image_manifest_digest(engine: &str, image: &str) -> Option<String> {
    if engine == "docker" {
        let out = Command::new("docker")
            .args([
                "image",
                "inspect",
                "-f",
                "{{if .RepoDigests}}{{index .RepoDigests 0}}{{end}}",
                image,
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        return first_sha256(&String::from_utf8_lossy(&out.stdout));
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

/// The content-addressed image tag for a tools layer atop a base image: `<prefix>-tools-<hash12>`
/// (`prefix` is the clean local image name, e.g. vhrn-claude — not the pulled registry ref,
/// which carries a colon and can't prefix a tag). The hash covers `base_id` (the base image's
/// identity) plus the normalized apt set and the ordered run list, so a rebuilt harness image
/// — or any change to the requested tooling — yields a fresh tag. Same inputs -> same tag,
/// built once.
#[cfg(test)]
fn tools_tag(prefix: &str, base_id: &str, apt: &[String], run: &[String]) -> String {
    let tools = NormalizedTools::new(apt, run);
    tools_tag_for(prefix, base_id, &tools)
}

fn tools_tag_for(prefix: &str, base_id: &str, tools: &NormalizedTools) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base_id.as_bytes());
    hasher.update(b"\napt\n");
    hasher.update(tools.apt.join("\n").as_bytes());
    hasher.update(b"\nrun\n");
    hasher.update(tools.run.join("\n").as_bytes());
    let hexed = hex::encode(hasher.finalize());
    format!("{prefix}-tools-{}", &hexed[..12])
}

/// A Dockerfile deriving an image FROM the harness image that bakes in the requested tools
/// at build time: an optional apt layer (as root, with list cleanup), then each `run`
/// command in declared order. Everything runs as root with `HOME=/home/dev` so user-space
/// installers (rustup, nvm) write into dev's home; a final chown hands ownership back to
/// dev. No sudo is ever introduced. PATH is deliberately not managed here — the entrypoint
/// sources `~/.profile` at runtime so each installer's own registration takes effect.
fn tools_dockerfile(base_image: &str, apt: &[String], run: &[String]) -> String {
    let mut lines = vec![
        format!("FROM {base_image}"),
        "USER root".to_string(),
        "ENV HOME=/home/dev".to_string(),
    ];
    let apt = normalize_apt(apt);
    if !apt.is_empty() {
        lines.push(format!(
            "RUN apt-get update && apt-get install -y --no-install-recommends {} \\\n    && rm -rf /var/lib/apt/lists/*",
            apt.join(" ")
        ));
    }
    for cmd in normalize_run(run) {
        lines.push(format!("RUN {cmd}"));
    }
    lines.push("RUN chown -R dev:dev /home/dev".to_string());
    let mut df = lines.join("\n");
    df.push('\n');
    df
}

// ---- tools local build (only the derived tools image is built locally;
// user-facing images are pulled) ---------------------------------------------------

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
/// content-addressed derived image (FROM `from_image`, tagged from the clean `tag_base`),
/// built once and cached by its tag. `from_image` is the pulled ref (the FROM); `tag_base`
/// is the clean image name — a ref with a colon can't prefix a tag.
pub(crate) fn ensure_tools_image(
    engine: &str,
    from_image: &str,
    tag_base: &str,
    apt: &[String],
    run: &[String],
) -> Result<String> {
    let tools = NormalizedTools::new(apt, run);
    if tools.is_empty() {
        return Ok(from_image.to_string());
    }
    // A newline inside an entry would split the generated `RUN <cmd>` line into invalid
    // Dockerfile; reject it up front with a clear message, not an opaque build parse error.
    for entry in tools.apt.iter().chain(tools.run.iter()) {
        if entry.contains('\n') {
            bail!(
                "[tools] entry spans multiple lines — chain with `&&` or a trailing `\\` in one entry: {entry:?}"
            );
        }
    }
    // Fold the base image's identity (its content digest, else the ref itself) into the
    // tag, so a rebuilt harness image forces a rebuild here even at an unchanged tag.
    let base_id = image_id(engine, from_image).unwrap_or_else(|| from_image.to_string());
    let tag = tools_tag_for(tag_base, &base_id, &tools);
    if image_exists(engine, &tag) {
        return Ok(tag);
    }
    let tmp = build_temp_dir()?;
    let dockerfile = tmp.join("Dockerfile");
    std::fs::write(
        &dockerfile,
        tools_dockerfile(from_image, &tools.apt, &tools.run),
    )?;
    let mut what = Vec::new();
    if !tools.apt.is_empty() {
        what.push(format!("apt: {}", tools.apt.join(", ")));
    }
    if !tools.run.is_empty() {
        what.push(format!("{} run step(s)", tools.run.len()));
    }
    info!("provisioning tools ({}) into {tag}...", what.join("; "));
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
    fn tools_tag_stable() {
        let a = tools_tag(
            "vhrn-claude",
            "sha256:aa",
            &["ripgrep".into(), "jq".into()],
            &["curl a | sh".into(), "curl b | sh".into()],
        );
        // apt: reorder + whitespace + dup must not change the tag.
        let b = tools_tag(
            "vhrn-claude",
            "sha256:aa",
            &["jq".into(), " ripgrep ".into(), "jq".into()],
            &["curl a | sh".into(), "curl b | sh".into()],
        );
        assert_eq!(a, b, "apt must be order/whitespace/dup independent");
        assert!(a.starts_with("vhrn-claude-tools-"), "unexpected tag {a}");
        // run order MUST matter — a later command can depend on an earlier one.
        let c = tools_tag(
            "vhrn-claude",
            "sha256:aa",
            &["ripgrep".into(), "jq".into()],
            &["curl b | sh".into(), "curl a | sh".into()],
        );
        assert_ne!(a, c, "run order must affect the tag");
        // A different apt set differs.
        assert_ne!(
            tools_tag("vhrn-claude", "sha256:aa", &["ripgrep".into()], &[]),
            tools_tag("vhrn-claude", "sha256:aa", &["jq".into()], &[]),
            "different apt sets should differ"
        );
        // A changed base image identity (a rebuilt harness) must change the tag.
        assert_ne!(
            tools_tag(
                "vhrn-claude",
                "sha256:bb",
                &["ripgrep".into(), "jq".into()],
                &["curl a | sh".into(), "curl b | sh".into()],
            ),
            a,
            "a new base image must force a new tools tag"
        );
    }

    #[test]
    fn normalized_tools_preserve_run_program_but_canonicalize_apt_set() {
        let tools = NormalizedTools::new(
            &[" jq ".into(), "ripgrep".into(), "jq".into(), String::new()],
            &[
                " first ".into(),
                String::new(),
                "first".into(),
                "second ".into(),
            ],
        );
        assert_eq!(tools.apt, vec!["jq", "ripgrep"]);
        assert_eq!(tools.run, vec!["first", "first", "second"]);
        let reordered = NormalizedTools::new(&tools.apt, &["second".into(), "first".into()]);
        assert_ne!(tools, reordered, "run order and duplicates are identity");
    }

    #[test]
    fn first_sha256_extracts_digest() {
        assert_eq!(
            first_sha256(r#"{"Id":"sha256:abcdef0123456789"}"#),
            Some("sha256:abcdef0123456789".to_string())
        );
        // A docker RepoDigests entry — what image_manifest_digest reads on the nightly path.
        assert_eq!(
            first_sha256("ghcr.io/aravind-n/vhrn-claude@sha256:c0a8ccd395b3848a"),
            Some("sha256:c0a8ccd395b3848a".to_string())
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
    fn tools_dockerfile_contents() {
        let df = tools_dockerfile(
            "vhrn-claude",
            &["ripgrep".into(), "jq".into()],
            &["curl https://sh.rustup.rs | sh -s -- -y".into()],
        );
        assert!(
            df.starts_with("FROM vhrn-claude\nUSER root\nENV HOME=/home/dev\n"),
            "header:\n{df}"
        );
        // apt sorted + wrapped with update and list cleanup.
        assert!(
            df.contains("apt-get install -y --no-install-recommends jq ripgrep"),
            "apt not sorted/wrapped:\n{df}"
        );
        assert!(
            df.contains("rm -rf /var/lib/apt/lists/*"),
            "apt cleanup:\n{df}"
        );
        // the run line verbatim, chown last, and no sudo anywhere.
        assert!(
            df.contains("RUN curl https://sh.rustup.rs | sh -s -- -y\n"),
            "run line:\n{df}"
        );
        assert!(
            df.trim_end().ends_with("RUN chown -R dev:dev /home/dev"),
            "chown must be last:\n{df}"
        );
        assert!(!df.contains("sudo"), "no sudo:\n{df}");
    }

    #[test]
    fn tools_dockerfile_apt_only_and_run_only() {
        // apt only: an apt layer, no stray RUN lines.
        let a = tools_dockerfile("base", &["jq".into()], &[]);
        assert!(a.contains("apt-get install"), "apt layer:\n{a}");
        // run only: no apt layer at all.
        let r = tools_dockerfile("base", &[], &["echo hi".into()]);
        assert!(!r.contains("apt-get"), "no apt layer when apt empty:\n{r}");
        assert!(r.contains("RUN echo hi\n"), "run line:\n{r}");
    }

    #[test]
    fn ensure_tools_image_no_tools_passes_through() {
        // No tools must pass the harness image through untouched, without touching the engine.
        let img = ensure_tools_image(
            "container",
            "ghcr.io/x/vhrn-claude:v1",
            "vhrn-claude",
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(img, "ghcr.io/x/vhrn-claude:v1");
    }

    #[test]
    fn ensure_tools_image_rejects_multiline_entry() {
        // A newline in an entry is refused before any engine call, with a clear error.
        let err = ensure_tools_image("container", "img", "base", &[], &["echo a\necho b".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple lines"), "unexpected error: {err}");
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
