//! The `validate` subcommand - offline config validation.

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
    fn validate_invalid_yaml() {
        let path = write_temp_config("validate_bad_yaml", "{{{{ not yaml");
        let args = ValidateArgs {
            config: path.clone(),
        };
        let result = run(&args);
        assert!(result.is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn validate_missing_tags_block() {
        let path = write_temp_config(
            "validate_no_tags",
            r#"
mappings:
  - from: library/nginx
"#,
        );
        let args = ValidateArgs {
            config: path.clone(),
        };
        let result = run(&args);
        assert!(result.is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn validate_unknown_source_registry() {
        let path = write_temp_config(
            "validate_bad_ref",
            r#"
registries:
  hub:
    url: registry-1.docker.io
mappings:
  - from: nginx
    source: nonexistent
    tags:
      glob: "*"
"#,
        );
        let args = ValidateArgs {
            config: path.clone(),
        };
        let result = run(&args);
        assert!(result.is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn validate_nonexistent_file() {
        let args = ValidateArgs {
            config: std::path::PathBuf::from("/tmp/ocync_test_does_not_exist.yaml"),
        };
        let result = run(&args);
        assert!(result.is_err());
    }
}
