//! OpenVEX statements for vulnerabilities a scanner matches on crates the
//! embedded dependency list names but the binary does not contain.
//!
//! Scanners that read the list `cargo auditable` embeds see those crates, so
//! the release ships a statement per matched vulnerability. Each names the
//! crate's versioned package URL as the product, the one form trivy and grype
//! both match in image, file and bill of materials scans.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Value, json};

/// The OpenVEX version these documents follow.
const CONTEXT: &str = "https://openvex.dev/ns/v0.2.0";

/// A crate the finished bill of materials dropped, and why.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Absent {
    /// The crate's package URL with its version, such as
    /// `pkg:cargo/ring@0.17.14`.
    pub(crate) purl: String,
    /// Why the binary does not contain it.
    pub(crate) reason: Reason,
}

/// Why a listed crate is not in the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    /// `cargo metadata` listed it through a weak feature; cargo does not
    /// compile it into `ocync`.
    NotCompiled,
    /// Cargo compiles it, but the binary links only the AWS-LC FIPS module.
    NotLinked,
}

impl Reason {
    fn impact(self) -> &'static str {
        match self {
            Self::NotCompiled => {
                "The dependency list embedded by cargo auditable comes from cargo metadata, which \
                 lists an optional dependency whenever a weak feature names it, even when nothing \
                 enables it. Cargo's resolver does not compile this crate into ocync, so the \
                 binary contains none of its code."
            }
            Self::NotLinked => {
                "rustls resolves aws-lc-sys in a FIPS build, but the binary links only the AWS-LC \
                 FIPS module: its only AWS-LC release string is the FIPS one."
            }
        }
    }
}

#[derive(Deserialize)]
struct Grype {
    #[serde(default)]
    matches: Vec<Match>,
}

#[derive(Deserialize)]
struct Match {
    vulnerability: Vulnerability,
    /// The same flaw under other identifiers, such as the CVE for a GitHub
    /// advisory. Scanners report different ones, so statements carry all.
    #[serde(default, rename = "relatedVulnerabilities")]
    related: Vec<Vulnerability>,
    artifact: Artifact,
}

#[derive(Deserialize)]
struct Vulnerability {
    id: String,
}

#[derive(Deserialize)]
struct Artifact {
    purl: Option<String>,
}

/// The OpenVEX document for `grype`'s matches on `absent` crates, or `None`
/// when no match concerns one. `id` names the document and `timestamp`
/// dates it.
pub(crate) fn document(
    grype: &str,
    absent: &[Absent],
    id: &str,
    timestamp: &str,
) -> Result<Option<String>, String> {
    let grype: Grype =
        serde_json::from_str(grype).map_err(|e| format!("not grype JSON output: {e}"))?;
    let mut statements: BTreeMap<(&str, &Absent), BTreeSet<&str>> = BTreeMap::new();
    for m in &grype.matches {
        let Some(purl) = m.artifact.purl.as_deref() else {
            continue;
        };
        let purl = purl.split(['?', '#']).next().unwrap_or_default();
        let Some(absent) = absent.iter().find(|a| a.purl == purl) else {
            continue;
        };
        let id = m.vulnerability.id.as_str();
        statements.entry((id, absent)).or_default().extend(
            m.related
                .iter()
                .map(|r| r.id.as_str())
                .filter(|alias| *alias != id),
        );
    }
    if statements.is_empty() {
        return Ok(None);
    }
    let statements: Vec<Value> = statements
        .into_iter()
        .map(|((vulnerability, absent), aliases)| {
            json!({
                "vulnerability": { "name": vulnerability, "aliases": aliases },
                "products": [{ "@id": absent.purl }],
                "status": "not_affected",
                "justification": "component_not_present",
                "impact_statement": absent.reason.impact(),
            })
        })
        .collect();
    let doc = json!({
        "@context": CONTEXT,
        "@id": id,
        "author": "ocync release workflow",
        "timestamp": timestamp,
        "version": 1,
        "statements": statements,
    });
    serde_json::to_string_pretty(&doc)
        .map(Some)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absent() -> Vec<Absent> {
        vec![
            Absent {
                purl: "pkg:cargo/ring@0.17.11".to_string(),
                reason: Reason::NotCompiled,
            },
            Absent {
                purl: "pkg:cargo/aws-lc-sys@0.46.0".to_string(),
                reason: Reason::NotLinked,
            },
        ]
    }

    fn grype(matches: &[(&str, &str)]) -> String {
        let matches: Vec<String> = matches
            .iter()
            .map(|(id, purl)| {
                format!(
                    r#"{{"vulnerability":{{"id":"{id}"}},"relatedVulnerabilities":[{{"id":"{id}"}},{{"id":"ALIAS-{id}"}}],"artifact":{{"purl":"{purl}"}}}}"#
                )
            })
            .collect();
        format!(r#"{{"matches":[{}]}}"#, matches.join(","))
    }

    #[test]
    fn states_each_match_on_an_absent_crate() {
        let grype = grype(&[
            ("RUSTSEC-2025-0009", "pkg:cargo/ring@0.17.11"),
            ("GHSA-4p46-pwfr-66x6", "pkg:cargo/ring@0.17.11?package-id=1"),
            ("CVE-2099-0001", "pkg:cargo/aws-lc-sys@0.46.0"),
        ]);
        let doc: Value = serde_json::from_str(
            &document(&grype, &absent(), "urn:test", "2026-10-24T00:00:00Z")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(doc["@context"], CONTEXT);
        let statements = doc["statements"].as_array().unwrap();
        let named: Vec<(&str, &str)> = statements
            .iter()
            .map(|s| {
                (
                    s["vulnerability"]["name"].as_str().unwrap(),
                    s["products"][0]["@id"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                ("CVE-2099-0001", "pkg:cargo/aws-lc-sys@0.46.0"),
                ("GHSA-4p46-pwfr-66x6", "pkg:cargo/ring@0.17.11"),
                ("RUSTSEC-2025-0009", "pkg:cargo/ring@0.17.11"),
            ]
        );
        assert_eq!(
            statements[2]["vulnerability"]["aliases"],
            json!(["ALIAS-RUSTSEC-2025-0009"])
        );
        assert!(statements.iter().all(
            |s| s["status"] == "not_affected" && s["justification"] == "component_not_present"
        ));
    }

    #[test]
    fn says_nothing_about_crates_the_binary_contains() {
        let grype = grype(&[("RUSTSEC-2099-0001", "pkg:cargo/rustls@0.23.46")]);
        assert_eq!(document(&grype, &absent(), "urn:test", "t"), Ok(None));
    }

    #[test]
    fn says_nothing_about_another_version_of_an_absent_crate() {
        let grype = grype(&[("RUSTSEC-2025-0009", "pkg:cargo/ring@0.17.10")]);
        assert_eq!(document(&grype, &absent(), "urn:test", "t"), Ok(None));
    }
}
