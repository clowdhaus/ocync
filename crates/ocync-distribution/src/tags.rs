use serde::Deserialize;

use crate::client::RegistryClient;
use crate::error::Error;

/// Response body from the tag listing API.
#[derive(Debug, Clone, Deserialize)]
pub struct TagListResponse {
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_null_as_empty")]
    pub tags: Vec<String>,
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
/// `<url>; rel="next"`.
pub fn parse_next_link(link_header: &str) -> Option<String> {
    for part in link_header.split(',') {
        let part = part.trim();
        // Check if this link relation contains rel="next".
        if !part.contains("rel=\"next\"") {
            continue;
        }
        // Extract URL from angle brackets.
        if let Some(start) = part.find('<') {
            if let Some(end) = part.find('>') {
                if start < end {
                    return Some(part[start + 1..end].to_owned());
                }
            }
        }
    }
    None
}

impl RegistryClient {
    /// List all tags in a repository, automatically following pagination.
    ///
    /// Returns a complete list of tags by following `Link: rel="next"` headers.
    pub async fn list_tags(&self, repository: &str) -> Result<Vec<String>, Error> {
        let mut all_tags = Vec::new();
        let mut path = "tags/list".to_owned();

        loop {
            let resp = self.get(repository, &path, None).await?;

            // Check for Link header before consuming the body.
            let next_url = resp
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_next_link);

            let tag_list: TagListResponse = resp.json().await?;
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
                        // Absolute URL — extract path+query after the /v2/{repo}/ prefix.
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
}
