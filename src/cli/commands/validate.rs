//! The `validate` subcommand: offline config validation.

use crate::ValidateArgs;
use crate::cli::config::load_config;
use crate::cli::{CliError, ExitCode};

pub(crate) fn run(args: &ValidateArgs) -> Result<ExitCode, CliError> {
    let config = load_config(&args.config)?;
    let n_mappings = config.mappings.len();
    let n_registries = config.registries.len();
    eprintln!(
        "config valid: {n_registries} registries, {n_mappings} mappings ({})",
        args.config.display(),
    );
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn write_temp_config(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("ocync_test_{name}.yaml"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn validate_valid_config() {
        let path = write_temp_config(
            "validate_valid",
            r#"
registries:
  hub:
    url: registry-1.docker.io
mappings:
  - from: library/nginx
    source: hub
    tags:
      glob: "*"
"#,
        );
        let args = ValidateArgs {
            config: path.clone(),
        };
        let result = run(&args).unwrap();
        assert_eq!(result, ExitCode::Success);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn validate_rejects_bad_configs() {
        let cases = [
            ("invalid yaml", Some("validate_bad_yaml"), "{{{{ not yaml"),
            (
                "missing tags block",
                Some("validate_no_tags"),
                "\nmappings:\n  - from: library/nginx\n",
            ),
            (
                "unknown source registry",
                Some("validate_bad_ref"),
                "\nregistries:\n  hub:\n    url: registry-1.docker.io\nmappings:\n  - from: nginx\n    source: nonexistent\n    tags:\n      glob: \"*\"\n",
            ),
            ("nonexistent file", None, ""),
        ];
        for (name, file, content) in cases {
            let path = match file {
                Some(file) => write_temp_config(file, content),
                None => std::path::PathBuf::from("/tmp/ocync_test_does_not_exist.yaml"),
            };
            let result = run(&ValidateArgs {
                config: path.clone(),
            });
            assert!(result.is_err(), "{name}");
            let _ = std::fs::remove_file(path);
        }
    }
}
