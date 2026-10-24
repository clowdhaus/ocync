//! Edit syft's CycloneDX document into the release bill of materials.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::Crypto;
use crate::binary::AwsLc;
use crate::resolver::{Compiled, Crate};

/// The package URL prefix CycloneDX gives a Rust crate. Image documents also
/// list OS packages, which carry other prefixes and are kept as they are.
const CARGO_PURL: &str = "pkg:cargo/";

/// The `bom-ref` of the AWS-LC library component this adds.
const AWS_LC_REF: &str = "aws-lc";

/// Keep only crates `compiled` lists, add the linked AWS-LC library, and
/// confirm the result describes this source tree's `ocync` and AWS-LC build.
pub(crate) fn finish(
    raw: &str,
    compiled: &Compiled,
    aws_lc: &AwsLc,
    crypto: Crypto,
) -> Result<String, String> {
    let mut doc: Value =
        serde_json::from_str(raw).map_err(|e| format!("not a CycloneDX document: {e}"))?;
    let components = doc
        .get_mut("components")
        .and_then(Value::as_array_mut)
        .ok_or("the document lists no components")?;

    let mut removed: BTreeSet<String> = BTreeSet::new();
    components.retain(|component| {
        let Some(krate) = cargo_crate(component) else {
            return true;
        };
        let keep = compiled.crates.contains(&krate)
            && crypto.resolved_not_linked() != Some(krate.name.as_str());
        if !keep && let Some(bom_ref) = component["bom-ref"].as_str() {
            removed.insert(bom_ref.to_string());
        }
        keep
    });

    let kept: Vec<Crate> = components.iter().filter_map(cargo_crate).collect();
    let missing: Vec<String> = [
        ("ocync", Some(compiled.ocync.as_str())),
        ("aws-lc-rs", None),
        (crypto.sys_crate(), None),
    ]
    .into_iter()
    .filter(|(name, version)| {
        !kept
            .iter()
            .any(|c| c.name == *name && version.is_none_or(|v| c.version == v))
    })
    .map(|(name, version)| match version {
        Some(v) => format!("`{name}@{v}`"),
        None => format!("`{name}`"),
    })
    .collect();
    if !missing.is_empty() {
        return Err(format!(
            "bill of materials ({} crates) is missing {}",
            kept.len(),
            missing.join(", ")
        ));
    }

    let sys_ref = components
        .iter()
        .find(|c| cargo_crate(c).is_some_and(|k| k.name == crypto.sys_crate()))
        .and_then(|c| c["bom-ref"].as_str())
        .map(str::to_string);
    components.push(aws_lc_component(aws_lc));

    if let Some(dependencies) = doc.get_mut("dependencies").and_then(Value::as_array_mut) {
        dependencies.retain(|d| !d["ref"].as_str().is_some_and(|r| removed.contains(r)));
        for dependency in dependencies.iter_mut() {
            if let Some(depends_on) = dependency
                .get_mut("dependsOn")
                .and_then(Value::as_array_mut)
            {
                depends_on.retain(|r| !r.as_str().is_some_and(|r| removed.contains(r)));
            }
            if sys_ref.is_some() && dependency["ref"].as_str() == sys_ref.as_deref() {
                match dependency
                    .get_mut("dependsOn")
                    .and_then(Value::as_array_mut)
                {
                    Some(depends_on) => depends_on.push(json!(AWS_LC_REF)),
                    None => dependency["dependsOn"] = json!([AWS_LC_REF]),
                }
            }
        }
    }

    serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())
}

fn cargo_crate(component: &Value) -> Option<Crate> {
    component["purl"].as_str()?.strip_prefix(CARGO_PURL)?;
    Some(Crate {
        name: component["name"].as_str()?.to_string(),
        version: component["version"].as_str()?.to_string(),
    })
}

/// The C library `aws-lc-fips-sys` or `aws-lc-sys` builds and links. A FIPS
/// release names its FIPS 140-3 module by its major version.
fn aws_lc_component(aws_lc: &AwsLc) -> Value {
    let mut component = json!({
        "bom-ref": AWS_LC_REF,
        "type": "library",
        "supplier": { "name": "Amazon Web Services" },
        "version": aws_lc.release,
    });
    match aws_lc.crypto {
        Crypto::Fips => {
            let module = aws_lc.release.split('.').next().unwrap_or_default();
            component["name"] = json!("AWS-LC FIPS");
            component["properties"] = json!([
                { "name": "ocync:fips-140-3-module", "value": format!("AWS-LC-FIPS {module}") }
            ]);
        }
        Crypto::NonFips => {
            component["name"] = json!("AWS-LC");
            component["purl"] = json!(format!("pkg:github/aws/aws-lc@v{}", aws_lc.release));
        }
    }
    component
}

#[cfg(test)]
mod tests {
    use super::*;

    fn krate(name: &str, version: &str) -> Crate {
        Crate {
            name: name.to_string(),
            version: version.to_string(),
        }
    }

    fn component(name: &str, version: &str) -> String {
        format!(
            r#"{{"bom-ref":"pkg:cargo/{name}@{version}?id","type":"library","name":"{name}","version":"{version}","purl":"pkg:cargo/{name}@{version}"}}"#
        )
    }

    fn raw(crates: &[(&str, &str)]) -> String {
        let components: Vec<String> = crates
            .iter()
            .map(|(n, v)| component(n, v))
            .chain([
                r#"{"bom-ref":"apk","type":"library","name":"ca-certificates-bundle","purl":"pkg:apk/wolfi/ca-certificates-bundle@1"}"#
                    .to_string(),
            ])
            .collect();
        format!(
            r#"{{"bomFormat":"CycloneDX","components":[{}],"dependencies":[
                {{"ref":"pkg:cargo/ocync@0.8.2?id","dependsOn":["pkg:cargo/ring@0.17.14?id","pkg:cargo/aws-lc-rs@1.18.2?id"]}},
                {{"ref":"pkg:cargo/ring@0.17.14?id","dependsOn":[]}},
                {{"ref":"pkg:cargo/aws-lc-fips-sys@0.14.3?id"}}
            ]}}"#,
            components.join(",")
        )
    }

    fn compiled(crates: &[(&str, &str)]) -> Compiled {
        Compiled {
            crates: crates.iter().map(|(n, v)| krate(n, v)).collect(),
            ocync: "0.8.2".to_string(),
        }
    }

    fn fips() -> AwsLc {
        AwsLc {
            crypto: Crypto::Fips,
            release: "4.2.0".to_string(),
        }
    }

    const FIPS_BUILD: &[(&str, &str)] = &[
        ("ocync", "0.8.2"),
        ("aws-lc-rs", "1.18.2"),
        ("aws-lc-fips-sys", "0.14.3"),
        ("aws-lc-sys", "0.46.0"),
    ];

    fn names(doc: &Value) -> Vec<&str> {
        doc["components"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect()
    }

    #[test]
    fn drops_crates_cargo_does_not_compile_and_keeps_os_packages() {
        let mut listed = FIPS_BUILD.to_vec();
        listed.push(("ring", "0.17.14"));
        let out = finish(&raw(&listed), &compiled(FIPS_BUILD), &fips(), Crypto::Fips).unwrap();
        let doc: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            names(&doc),
            [
                "ocync",
                "aws-lc-rs",
                "aws-lc-fips-sys",
                "ca-certificates-bundle",
                "AWS-LC FIPS"
            ]
        );
    }

    #[test]
    fn removes_dependency_edges_to_dropped_crates() {
        let mut listed = FIPS_BUILD.to_vec();
        listed.push(("ring", "0.17.14"));
        let out = finish(&raw(&listed), &compiled(FIPS_BUILD), &fips(), Crypto::Fips).unwrap();
        assert!(!out.contains("ring@0.17.14"), "{out}");
        let doc: Value = serde_json::from_str(&out).unwrap();
        let sys = doc["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["ref"] == "pkg:cargo/aws-lc-fips-sys@0.14.3?id")
            .unwrap();
        assert_eq!(sys["dependsOn"], json!([AWS_LC_REF]));
    }

    #[test]
    fn names_the_fips_module() {
        let out = finish(
            &raw(FIPS_BUILD),
            &compiled(FIPS_BUILD),
            &fips(),
            Crypto::Fips,
        )
        .unwrap();
        let doc: Value = serde_json::from_str(&out).unwrap();
        let aws_lc = doc["components"].as_array().unwrap().last().unwrap();
        assert_eq!(aws_lc["version"], "4.2.0");
        assert_eq!(aws_lc["properties"][0]["value"], "AWS-LC-FIPS 4");
        assert!(aws_lc.get("purl").is_none());
    }

    #[test]
    fn names_the_non_fips_release_by_its_tag() {
        let build = [
            ("ocync", "0.8.2"),
            ("aws-lc-rs", "1.18.2"),
            ("aws-lc-sys", "0.46.0"),
        ];
        let aws_lc = AwsLc {
            crypto: Crypto::NonFips,
            release: "5.11.0".to_string(),
        };
        let out = finish(&raw(&build), &compiled(&build), &aws_lc, Crypto::NonFips).unwrap();
        let doc: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            names(&doc),
            [
                "ocync",
                "aws-lc-rs",
                "aws-lc-sys",
                "ca-certificates-bundle",
                "AWS-LC"
            ]
        );
        assert_eq!(
            doc["components"].as_array().unwrap().last().unwrap()["purl"],
            "pkg:github/aws/aws-lc@v5.11.0"
        );
    }

    #[test]
    fn rejects_a_binary_built_without_its_dependency_list() {
        assert_eq!(
            finish(&raw(&[]), &compiled(FIPS_BUILD), &fips(), Crypto::Fips),
            Err(
                "bill of materials (0 crates) is missing `ocync@0.8.2`, `aws-lc-rs`, `aws-lc-fips-sys`"
                    .to_string()
            )
        );
    }

    #[test]
    fn rejects_a_binary_from_another_version() {
        let mut listed = FIPS_BUILD.to_vec();
        listed[0] = ("ocync", "0.8.1");
        let mut build = FIPS_BUILD.to_vec();
        build.push(("ocync", "0.8.1"));
        assert_eq!(
            finish(&raw(&listed), &compiled(&build), &fips(), Crypto::Fips),
            Err("bill of materials (3 crates) is missing `ocync@0.8.2`".to_string())
        );
    }

    #[test]
    fn rejects_a_fips_build_without_the_fips_module() {
        let listed = [
            ("ocync", "0.8.2"),
            ("aws-lc-rs", "1.18.2"),
            ("aws-lc-sys", "0.46.0"),
        ];
        assert_eq!(
            finish(&raw(&listed), &compiled(FIPS_BUILD), &fips(), Crypto::Fips),
            Err("bill of materials (2 crates) is missing `aws-lc-fips-sys`".to_string())
        );
    }
}
