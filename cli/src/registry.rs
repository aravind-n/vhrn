//! Registry queries for `vhrn update`: ask an OCI registry what is published so the CLI can
//! decide whether to pull, instead of pulling to find out. Uses the standard bearer-challenge
//! flow (GET, on 401 parse `WWW-Authenticate`, fetch a token, retry), so it is
//! registry-agnostic — GHCR, Docker Hub, any OCI registry. Network reads are best-effort:
//! any failure returns `None` and the caller reports the harness as uncheckable.

use std::time::Duration;

use serde::Deserialize;

/// A registry manifest `Accept` header covering the multi-arch index and single manifest
/// media types, so `Docker-Content-Digest` names the same digest the engine stores locally.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

/// A parsed `WWW-Authenticate: Bearer realm=…,service=…,scope=…` challenge.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BearerChallenge {
    pub realm: String,
    pub service: String,
    pub scope: String,
}

/// Parse the `Bearer realm="…",service="…",scope="…"` header a registry returns on a 401.
/// Reads each quoted value by name so an embedded comma (e.g. `scope="…:pull,push"`) is safe.
/// Needs a realm; service/scope default to empty.
pub(crate) fn parse_www_authenticate(header: &str) -> Option<BearerChallenge> {
    // The auth scheme is case-insensitive (RFC 7235), so don't insist on "Bearer".
    let scheme_is_bearer = header
        .trim_start()
        .get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case("bearer"));
    if !scheme_is_bearer {
        return None;
    }
    Some(BearerChallenge {
        realm: quoted_value(header, "realm")?,
        service: quoted_value(header, "service").unwrap_or_default(),
        scope: quoted_value(header, "scope").unwrap_or_default(),
    })
}

/// The value of a `key="value"` pair in a header, or `None` if the key is absent.
fn quoted_value(header: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = header.find(&needle)? + needle.len();
    let end = header[start..].find('"')?;
    Some(header[start..start + end].to_string())
}

/// The token endpoint for a challenge: `realm?service=…&scope=…` (our pull scopes are already
/// URL-safe, so no escaping is needed).
fn token_url(bc: &BearerChallenge) -> String {
    let mut params = Vec::new();
    if !bc.service.is_empty() {
        params.push(format!("service={}", bc.service));
    }
    if !bc.scope.is_empty() {
        params.push(format!("scope={}", bc.scope));
    }
    if params.is_empty() {
        bc.realm.clone()
    } else {
        format!("{}?{}", bc.realm, params.join("&"))
    }
}

/// Split a registry base like `ghcr.io/aravind-n` into (host, namespace). A base with no `/`
/// is all host, empty namespace.
fn split_registry(base: &str) -> (&str, &str) {
    base.split_once('/').unwrap_or((base, ""))
}

/// The repository path for `image` on a registry base: `<namespace>/<image>`, or just
/// `<image>` when the base carries no namespace.
fn repo_path(base: &str, image: &str) -> String {
    let (_, ns) = split_registry(base);
    if ns.is_empty() {
        image.to_string()
    } else {
        format!("{ns}/{image}")
    }
}

/// Parse a strict `X.Y.Z` tag into a comparable tuple. Anything with a suffix (`2.1.3-x`),
/// a moving tag (`nightly`, `latest`), or non-numeric parts is rejected.
fn parse_semver(tag: &str) -> Option<(u64, u64, u64)> {
    let mut it = tag.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None; // more than three components
    }
    Some((major, minor, patch))
}

/// The newest strict-`X.Y.Z` tag in a tag list, returned in its original string form. Tags
/// that are not strict semver (`latest`, `nightly-…`, `2.1.3-20260101`) are ignored.
pub(crate) fn newest_semver(tags: &[String]) -> Option<String> {
    tags.iter()
        .filter_map(|t| parse_semver(t).map(|v| (v, t)))
        .max_by_key(|(v, _)| *v)
        .map(|(_, t)| t.clone())
}

/// Whether `newest` is a strictly newer version than `installed`. `None` if either is not
/// strict semver, so the caller can distinguish "can't tell" from "not newer".
pub(crate) fn update_available(newest: &str, installed: &str) -> Option<bool> {
    Some(parse_semver(newest)? > parse_semver(installed)?)
}

// ---- network edge (best-effort; any failure → None) --------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    tags: Vec<String>,
}

/// An agent that surfaces non-2xx as `Ok` (so a 401's challenge header is readable) and caps
/// each call so a hung registry can't stall the CLI.
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into()
}

/// GET `url` doing the OCI bearer-challenge transparently: on a 401, parse the challenge,
/// fetch a token, and retry once with it. Returns the final response.
fn get_challenged(
    agent: &ureq::Agent,
    url: &str,
    accept: Option<&str>,
) -> Option<ureq::http::Response<ureq::Body>> {
    let mut first = agent.get(url);
    if let Some(a) = accept {
        first = first.header("Accept", a);
    }
    let resp = first.call().ok()?;
    if resp.status().as_u16() != 401 {
        return Some(resp);
    }
    let challenge = resp.headers().get("www-authenticate")?.to_str().ok()?;
    let token = fetch_token(agent, &parse_www_authenticate(challenge)?)?;
    let mut retry = agent
        .get(url)
        .header("Authorization", &format!("Bearer {token}"));
    if let Some(a) = accept {
        retry = retry.header("Accept", a);
    }
    retry.call().ok()
}

/// Fetch an anonymous bearer token from the challenge's realm.
fn fetch_token(agent: &ureq::Agent, bc: &BearerChallenge) -> Option<String> {
    let mut resp = agent.get(&token_url(bc)).call().ok()?;
    if resp.status().as_u16() != 200 {
        return None;
    }
    let body = resp.body_mut().read_to_string().ok()?;
    serde_json::from_str::<TokenResponse>(&body)
        .ok()
        .map(|t| t.token)
}

/// The `rel="next"` target of an OCI pagination `Link` header, resolved to an absolute URL.
fn link_next(link: Option<&str>, host: &str) -> Option<String> {
    let link = link?;
    for part in link.split(',') {
        if !part.contains("rel=\"next\"") && !part.contains("rel=next") {
            continue;
        }
        let start = part.find('<')? + 1;
        let end = part[start..].find('>')? + start;
        let target = &part[start..end];
        return Some(if target.starts_with("http") {
            target.to_string()
        } else {
            format!("https://{host}{target}")
        });
    }
    None
}

/// Every tag published for `image` on `registry`, following `Link` pagination (capped).
fn fetch_tags(registry: &str, image: &str) -> Option<Vec<String>> {
    let host = split_registry(registry).0;
    let repo = repo_path(registry, image);
    let agent = http_agent();
    let mut url = format!("https://{host}/v2/{repo}/tags/list");
    let mut all = Vec::new();
    for _ in 0..50 {
        let resp = get_challenged(&agent, &url, None)?;
        if resp.status().as_u16() != 200 {
            return (!all.is_empty()).then_some(all);
        }
        let next = link_next(
            resp.headers().get("link").and_then(|h| h.to_str().ok()),
            host,
        );
        let body = resp.into_body().read_to_string().ok()?;
        all.append(&mut serde_json::from_str::<TagsResponse>(&body).ok()?.tags);
        match next {
            Some(n) => url = n,
            None => break,
        }
    }
    Some(all)
}

/// The newest published strict-`X.Y.Z` version for a harness image, or `None` if the registry
/// can't be reached or has no semver tag.
pub(crate) fn newest_published_version(registry: &str, image: &str) -> Option<String> {
    newest_semver(&fetch_tags(registry, image)?)
}

/// The manifest digest (`sha256:…`) a tag resolves to in the registry — the same value the
/// engine stores locally, for the nightly digest comparison. `None` on any failure.
pub(crate) fn remote_manifest_digest(registry: &str, image: &str, tag: &str) -> Option<String> {
    let host = split_registry(registry).0;
    let repo = repo_path(registry, image);
    let url = format!("https://{host}/v2/{repo}/manifests/{tag}");
    let resp = get_challenged(&http_agent(), &url, Some(MANIFEST_ACCEPT))?;
    if resp.status().as_u16() != 200 {
        return None;
    }
    let digest = resp
        .headers()
        .get("docker-content-digest")?
        .to_str()
        .ok()?
        .trim()
        .to_string();
    digest.starts_with("sha256:").then_some(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The real challenge strings GHCR and Docker Hub return on an unauthenticated /v2 read.
    #[test]
    fn parse_challenge_ghcr_and_dockerhub() {
        let ghcr = parse_www_authenticate(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:aravind-n/vhrn-claude:pull""#,
        )
        .expect("ghcr challenge");
        assert_eq!(ghcr.realm, "https://ghcr.io/token");
        assert_eq!(ghcr.service, "ghcr.io");
        assert_eq!(ghcr.scope, "repository:aravind-n/vhrn-claude:pull");

        let hub = parse_www_authenticate(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/node:pull""#,
        )
        .expect("docker hub challenge");
        assert_eq!(hub.realm, "https://auth.docker.io/token");
        assert_eq!(hub.service, "registry.docker.io");
        assert_eq!(hub.scope, "repository:library/node:pull");
    }

    #[test]
    fn parse_challenge_rejects_non_bearer() {
        assert!(parse_www_authenticate(r#"Basic realm="x""#).is_none());
        assert!(parse_www_authenticate("Bearer service=\"x\"").is_none()); // no realm
        // The scheme is case-insensitive (RFC 7235).
        assert_eq!(
            parse_www_authenticate(r#"bearer realm="https://r/token""#).map(|c| c.realm),
            Some("https://r/token".to_string())
        );
    }

    #[test]
    fn token_url_builds_query() {
        let bc = BearerChallenge {
            realm: "https://auth.docker.io/token".into(),
            service: "registry.docker.io".into(),
            scope: "repository:library/node:pull".into(),
        };
        assert_eq!(
            token_url(&bc),
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/node:pull"
        );
        // A realm without service/scope stays bare.
        let bare = BearerChallenge {
            realm: "https://ghcr.io/token".into(),
            service: String::new(),
            scope: String::new(),
        };
        assert_eq!(token_url(&bare), "https://ghcr.io/token");
    }

    #[test]
    fn repo_path_from_registry_base() {
        assert_eq!(
            split_registry("ghcr.io/aravind-n"),
            ("ghcr.io", "aravind-n")
        );
        assert_eq!(
            repo_path("ghcr.io/aravind-n", "vhrn-claude"),
            "aravind-n/vhrn-claude"
        );
        assert_eq!(
            repo_path("ghcr.io/org/sub", "vhrn-claude"),
            "org/sub/vhrn-claude"
        );
        // A bare host with no namespace.
        assert_eq!(repo_path("localhost:5000", "vhrn-claude"), "vhrn-claude");
    }

    #[test]
    fn parse_semver_is_strict() {
        assert_eq!(parse_semver("2.1.218"), Some((2, 1, 218)));
        assert_eq!(parse_semver("2.1.218-20260724"), None); // dated suffix
        assert_eq!(parse_semver("nightly"), None);
        assert_eq!(parse_semver("latest"), None);
        assert_eq!(parse_semver("2.1"), None); // too few
        assert_eq!(parse_semver("2.1.3.4"), None); // too many
        assert_eq!(parse_semver("2.1.x"), None);
    }

    #[test]
    fn newest_semver_picks_max_ignoring_non_semver() {
        let tags: Vec<String> = [
            "nightly",
            "2.1.218",
            "latest",
            "2.1.218-20260724",
            "2.10.0",
            "2.9.9",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(newest_semver(&tags).as_deref(), Some("2.10.0"));
        assert_eq!(newest_semver(&["nightly".into(), "latest".into()]), None);
        assert_eq!(newest_semver(&[]), None);
    }

    #[test]
    fn update_available_compares_semver() {
        assert_eq!(update_available("2.1.219", "2.1.218"), Some(true));
        assert_eq!(update_available("2.1.218", "2.1.218"), Some(false));
        assert_eq!(update_available("2.1.217", "2.1.218"), Some(false));
        assert_eq!(update_available("2.2.0", "2.1.999"), Some(true));
        assert_eq!(update_available("nightly", "2.1.218"), None);
    }

    #[test]
    fn link_next_resolves_relative_and_absolute() {
        assert_eq!(
            link_next(
                Some(r#"</v2/x/tags/list?last=z&n=100>; rel="next""#),
                "ghcr.io"
            ),
            Some("https://ghcr.io/v2/x/tags/list?last=z&n=100".into())
        );
        assert_eq!(
            link_next(Some(r#"<https://other/next>; rel="next""#), "ghcr.io"),
            Some("https://other/next".into())
        );
        assert_eq!(link_next(None, "ghcr.io"), None);
        assert_eq!(link_next(Some("<x>; rel=\"prev\""), "ghcr.io"), None);
    }
}
