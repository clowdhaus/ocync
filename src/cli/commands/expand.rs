//! The `expand` subcommand: shows resolved config with env var expansion.

use crate::ExpandArgs;
use crate::cli::config::{SecretVars, expand_config_for_display};
use crate::cli::{CliError, ExitCode};

/// Print the config with `${VAR}` expressions expanded.
pub(crate) fn run(args: &ExpandArgs) -> Result<ExitCode, CliError> {
    print!("{}", expand(args)?);
    Ok(ExitCode::Success)
}

/// The expanded config text; secret-pattern variables are rejected unless
/// `--show-secrets` is set.
fn expand(args: &ExpandArgs) -> Result<String, CliError> {
    let secrets = if args.show_secrets {
        SecretVars::Allow
    } else {
        SecretVars::Block
    };
    Ok(expand_config_for_display(&args.config, secrets)?)
}

#[cfg(test)]
mod tests {
    use super::expand;
    use crate::ExpandArgs;
    use crate::cli::commands::test_support::write_temp_config;
    use crate::cli::config::{SecretVars, expand_config_for_display};

    #[test]
    fn expand_resolves_env_vars() {
        let path = write_temp_config(
            "expand_env",
            r#"
registries:
  ecr:
    url: "${OCYNC_TEST_ECR_URL}"
mappings:
  - from: nginx
    tags:
      glob: "*"
"#,
        );
        // SAFETY: test is single-threaded and restores the original value.
        unsafe {
            std::env::set_var(
                "OCYNC_TEST_ECR_URL",
                "123456.dkr.ecr.us-east-1.amazonaws.com",
            )
        };
        let expanded = expand_config_for_display(&path, SecretVars::Allow).unwrap();
        assert!(expanded.contains("123456.dkr.ecr.us-east-1.amazonaws.com"));
        assert!(!expanded.contains("OCYNC_TEST_ECR_URL"));
        unsafe { std::env::remove_var("OCYNC_TEST_ECR_URL") };
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn show_secrets_flag_controls_secret_variables() {
        let path = write_temp_config(
            "expand_show_secrets",
            "token: \"${OCYNC_EXPAND_FLAG_SECRET}\"\n",
        );
        unsafe { std::env::set_var("OCYNC_EXPAND_FLAG_SECRET", "s3cret") };
        let cases = [
            ("flag off blocks", false, false),
            ("flag on shows", true, true),
        ];
        for (name, show_secrets, shown) in cases {
            let result = expand(&ExpandArgs {
                config: path.clone(),
                show_secrets,
            });
            if shown {
                assert_eq!(result.unwrap(), "token: \"s3cret\"\n", "{name}");
            } else {
                assert!(result.is_err(), "{name}");
            }
        }
        unsafe { std::env::remove_var("OCYNC_EXPAND_FLAG_SECRET") };
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expand_nonexistent_file() {
        let result = expand_config_for_display(
            std::path::Path::new("/tmp/ocync_test_missing.yaml"),
            SecretVars::Block,
        );
        assert!(result.is_err());
    }
}
