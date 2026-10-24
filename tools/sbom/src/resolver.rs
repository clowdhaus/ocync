//! The crates cargo's resolver compiles into `ocync` for one build, from
//! `cargo tree`, which resolves features for the one package.

use std::collections::BTreeSet;
use std::process::Command;

use crate::Crypto;

/// A crate name and version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Crate {
    pub(crate) name: String,
    pub(crate) version: String,
}

/// What cargo compiles into `ocync` for the host target.
pub(crate) struct Compiled {
    /// Every crate with a normal dependency edge, proc-macros included.
    pub(crate) crates: BTreeSet<Crate>,
    /// The `ocync` version in this source tree.
    pub(crate) ocync: String,
}

/// Ask cargo which crates it compiles into `ocync` for `crypto` on the
/// host target.
pub(crate) fn compiled(crypto: Crypto) -> Result<Compiled, String> {
    let host = host()?;
    let output = Command::new("cargo")
        .args([
            "tree", "--locked", "-p", "ocync", "-e", "normal", "--target",
        ])
        .arg(&host)
        .args(["--prefix", "none", "--format", "{p}"])
        .args(crypto.cargo_features())
        .output()
        .map_err(|e| format!("cargo tree: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

fn host() -> Result<String, String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|e| format!("rustc -vV: {e}"))?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_string)
        .ok_or_else(|| "rustc -vV printed no host".to_string())
}

/// Parse `cargo tree --prefix none --format {p}` lines such as
/// `clap v4.6.7` or `ocync v0.8.2 (/src)`. The first line is the root.
fn parse(text: &str) -> Result<Compiled, String> {
    let crates: BTreeSet<Crate> = text
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            let version = words.next()?.strip_prefix('v')?;
            Some(Crate {
                name: name.to_string(),
                version: version.to_string(),
            })
        })
        .collect();
    let ocync = crates
        .iter()
        .find(|c| c.name == "ocync")
        .map(|c| c.version.clone())
        .ok_or_else(|| "cargo tree did not list ocync".to_string())?;
    Ok(Compiled { crates, ocync })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_tree_lines() {
        let text = "ocync v0.8.2 (/src)\nclap v4.6.7\nclap v4.6.7 (*)\nserde_derive v1.0.229 (proc-macro)\n";
        let compiled = parse(text).unwrap();
        assert_eq!(compiled.ocync, "0.8.2");
        let names: Vec<String> = compiled
            .crates
            .iter()
            .map(|c| format!("{}@{}", c.name, c.version))
            .collect();
        assert_eq!(names, ["clap@4.6.7", "ocync@0.8.2", "serde_derive@1.0.229"]);
    }

    #[test]
    fn rejects_output_without_ocync() {
        assert!(parse("clap v4.6.7\n").is_err());
    }
}
