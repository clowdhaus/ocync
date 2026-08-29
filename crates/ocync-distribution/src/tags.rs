//! Tag listing with pagination for OCI repositories.

use std::cell::Cell;

use serde::Deserialize;

use crate::aimd::RegistryAction;
use crate::client::RegistryClient;
use crate::error::Error;
use crate::retry::{MAX_LISTING_RETRIES, retrying};
use crate::spec::RepositoryName;

/// Maximum number of tag list pages to follow before treating the pagination
/// as broken (e.g. a registry returning a self-referencing `Link: rel="next"`).
const MAX_TAG_PAGES: usize = 10_000;

// The tag listing deliberately sends no `n`.
//
// Asking for a page size can only cost round trips, never save them, because
// the registries answer a request without `n` at least as generously as one
// with it. Measured 2026-08-28:
//
// - Docker Hub returns every tag in one response with no `Link` header
//   (`library/python`, 3,911 tags, 1 request). With `n=1000` the same
//   listing takes 4.
// - `distribution/distribution` sets its internal limit to -1 when `n` is
//   absent and lists all tags; `maxtags` is applied only to an explicit `n`.
//   Harbor, GitLab, Quay and self-hosted `registry:2` inherit that.
// - ECR Public already pages at 1000 without being asked
//   (`docker/library/python`), and ECR rejects an `n` above 1000, so the
//   largest value it would accept changes nothing.
//
// So there is no registry where sending `n` helps and two large families
// where it multiplies the request count. The `Link` loop below still follows
// pagination for the registries that choose to impose it.

/// Response body from the tag listing API.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TagListResponse {
    /// The repository name (required by the spec but unused after deserialization).
    #[allow(dead_code)]
    name: String,
    /// The list of tags, or empty if none exist.
    #[serde(default, deserialize_with = "deserialize_null_as_empty")]
    pub(crate) tags: Vec<String>,
}

/// Deserialize a `Vec<String>` that may be `null` or missing as an empty vec.
fn deserialize_null_as_empty<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<Vec<String>> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// Parse a `Link` header to extract the URL for the `rel="next"` page.
///
/// The OCI Distribution spec uses RFC 5988 Link headers for pagination:
/// `<url>; rel="next"`. URLs are extracted from `<...>` delimiters first
/// to avoid splitting on commas that may appear within the URL itself.
pub(crate) fn parse_next_link(link_header: &str) -> Option<String> {
    let mut remaining = link_header;
    while !remaining.is_empty() {
        // Find the next link-value: starts with '<', URL ends at '>'.
        let Some(start) = remaining.find('<') else {
            break;
        };
        let after_start = &remaining[start + 1..];
        let Some(end) = after_start.find('>') else {
            break;
        };
        let url = &after_start[..end];
        let after_url = &after_start[end + 1..];

        // Find the end of this link-value (next '<' that starts a new link).
        let attrs_end = after_url.find('<').unwrap_or(after_url.len());
        let attrs = &after_url[..attrs_end];

        // Check if the attributes contain rel="next".
        if attrs.contains("rel=\"next\"") {
            return Some(url.to_owned());
        }

        remaining = &after_url[attrs_end..];
    }
    None
}

impl RegistryClient {
    /// List all tags in a repository, automatically following pagination.
    ///
    /// Returns a complete list of tags by following `Link: rel="next"` headers.
    pub async fn list_tags(&self, repository: &RepositoryName) -> Result<Vec<String>, Error> {
        let mut all_tags = Vec::new();
        let mut path = "tags/list".to_owned();
        let mut page_count: usize = 0;
        // One budget for the whole walk. Per-page retries would bound the
        // worst case only nominally: a listing may run to MAX_TAG_PAGES, and
        // four retries on each of those is hours of backing off at a registry
        // that has stopped serving the listing.
        let retry_budget = Cell::new(MAX_LISTING_RETRIES);

        loop {
            page_count += 1;
            if page_count > MAX_TAG_PAGES {
                return Err(Error::RegistryProtocol {
                    reason: format!(
                        "tag listing exceeded {MAX_TAG_PAGES} pages; possible infinite loop from malformed Link headers"
                    ),
                });
            }
            // Retried as one unit, body included. A throttle or a dropped
            // connection here is transient, and nothing above the prepare
            // phase would re-send it: the mapping would just fail. Reading
            // the body inside the retry matters because a connection reset
            // part way through a page is exactly the failure a re-send fixes.
            let (next_url, tag_list) = retrying("tag listing", &retry_budget, || async {
                let resp = self
                    .get(repository, &path, None, RegistryAction::TagList)
                    .await?;

                // Check for the Link header before consuming the body.
                let next_url = resp
                    .headers()
                    .get("link")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_next_link);

                let tag_list: TagListResponse = resp.json().await?;
                Ok((next_url, tag_list))
            })
            .await?;
            all_tags.extend(tag_list.tags);

            match next_url {
                Some(url) => {
                    // The Link header may be an absolute URL or a relative path.
                    // We need to extract just the path+query portion for build_url
                    // since the client's get() method builds a full URL.
                    //
                    // For relative URLs like `/v2/repo/tags/list?n=100&last=tag`,
                    // strip the `/v2/{repository}/` prefix.
                    let prefix = format!("/v2/{repository}/");
                    if let Some(stripped) = url.strip_prefix(&prefix) {
                        path = stripped.to_owned();
                    } else if url.starts_with("http://") || url.starts_with("https://") {
                        // Absolute URL - extract path+query after the /v2/{repo}/ prefix.
                        if let Ok(parsed) = url::Url::parse(&url) {
                            let full_path = parsed.path();
                            if let Some(stripped) = full_path.strip_prefix(&prefix) {
                                path = match parsed.query() {
                                    Some(q) => format!("{stripped}?{q}"),
                                    None => stripped.to_owned(),
                                };
                            } else {
                                // Fallback: use the full path as-is.
                                path = match parsed.query() {
                                    Some(q) => format!("{}?{q}", &full_path[1..]),
                                    None => full_path[1..].to_owned(),
                                };
                            }
                        } else {
                            break;
                        }
                    } else {
                        // Plain relative path.
                        path = url;
                    }
                }
                None => break,
            }
        }

        Ok(all_tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tag_list_response() {
        let json = r#"{"name": "library/nginx", "tags": ["latest", "1.25", "1.24"]}"#;
        let resp: TagListResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.name, "library/nginx");
        assert_eq!(resp.tags, vec!["latest", "1.25", "1.24"]);
    }

    #[test]
    fn parse_tag_list_response_empty_tags() {
        let json = r#"{"name": "library/nginx", "tags": []}"#;
        let resp: TagListResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.name, "library/nginx");
        assert!(resp.tags.is_empty());
    }

    #[test]
    fn parse_tag_list_response_null_tags() {
        // Some registries return null instead of an empty array.
        let json = r#"{"name": "library/nginx", "tags": null}"#;
        let resp: TagListResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.name, "library/nginx");
        assert!(resp.tags.is_empty());
    }

    #[test]
    fn parse_tag_list_response_missing_tags() {
        // serde(default) handles missing field.
        let json = r#"{"name": "library/nginx"}"#;
        let resp: TagListResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.name, "library/nginx");
        assert!(resp.tags.is_empty());
    }

    #[test]
    fn parse_next_link_with_next() {
        let header = r#"</v2/repo/tags/list?n=100&last=tag99>; rel="next""#;
        let result = parse_next_link(header);
        assert_eq!(
            result,
            Some("/v2/repo/tags/list?n=100&last=tag99".to_owned())
        );
    }

    #[test]
    fn parse_next_link_without_next() {
        let header = r#"</v2/repo/tags/list?n=100>; rel="previous""#;
        let result = parse_next_link(header);
        assert!(result.is_none());
    }

    #[test]
    fn parse_next_link_empty() {
        let result = parse_next_link("");
        assert!(result.is_none());
    }

    #[test]
    fn parse_next_link_multiple_rels() {
        let header = r#"</v2/repo/tags/list?n=100&last=a>; rel="prev", </v2/repo/tags/list?n=100&last=z>; rel="next""#;
        let result = parse_next_link(header);
        assert_eq!(result, Some("/v2/repo/tags/list?n=100&last=z".to_owned()));
    }

    #[test]
    fn parse_next_link_url_containing_comma() {
        // A URL with a comma must not be split incorrectly.
        let header = r#"</v2/repo/tags/list?filter=a,b&last=z>; rel="next""#;
        let result = parse_next_link(header);
        assert_eq!(
            result,
            Some("/v2/repo/tags/list?filter=a,b&last=z".to_owned())
        );
    }

    /// A registry that always returns the same `Link: rel="next"` URL must be
    /// stopped by the page-count guard, not loop forever.
    #[tokio::test]
    async fn tag_pagination_guard_fires() {
        let server = wiremock::MockServer::start().await;

        // Every request returns one tag and a self-referencing Link header.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(r"/v2/repo/tags/list.*"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"name": "repo", "tags": ["t"]}))
                    .append_header("link", r#"</v2/repo/tags/list?n=1&last=t>; rel="next""#),
            )
            .mount(&server)
            .await;

        let base_url = url::Url::parse(&server.uri()).unwrap();
        let client = crate::client::RegistryClientBuilder::new(base_url)
            .build()
            .unwrap();

        let repo = RepositoryName::new("repo").unwrap();
        let err = client.list_tags(&repo).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exceeded") && msg.contains("pages"),
            "unexpected error: {msg}"
        );
    }
}
