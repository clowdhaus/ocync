//! The `validate` subcommand: offline config validation.

use crate::ValidateArgs;
use crate::cli::config::load_config;
use crate::cli::{CliError, ExitCode};

/// Load and validate the config without contacting any registry.
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
    use super::*;
    use crate::cli::test_support::write_temp_config;

    #[test]
    fn validate_valid_config() {
        let file = write_temp_config(
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
            config: file.path().to_path_buf(),
        };
        let result = run(&args).unwrap();
        assert_eq!(result, ExitCode::Success);
    }

    #[test]
    fn validate_rejects_bad_configs() {
        // (name, config text or `None` for a missing file, error substring)
        let cases = [
            ("invalid yaml", Some("{{{{ not yaml"), "failed to parse"),
            (
                "missing tags block",
                Some("\nmappings:\n  - from: library/nginx\n"),
                "missing a tags block",
            ),
            (
                "unknown source registry",
                Some(
                    "\nregistries:\n  hub:\n    url: registry-1.docker.io\nmappings:\n  - from: nginx\n    source: nonexistent\n    tags:\n      glob: \"*\"\n",
                ),
                "unknown registry 'nonexistent'",
            ),
            ("nonexistent file", None, "failed to read"),
        ];
        for (name, content, want) in cases {
            let file = content.map(write_temp_config);
            let config = match &file {
                Some(file) => file.path().to_path_buf(),
                None => std::path::PathBuf::from("/tmp/ocync_test_does_not_exist.yaml"),
            };
            let err = run(&ValidateArgs { config }).unwrap_err().to_string();
            assert!(err.contains(want), "{name}: expected '{want}' in: {err}");
        }
    }
}
