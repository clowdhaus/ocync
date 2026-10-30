//! The `expand` subcommand: shows resolved config with env var expansion.

use crate::ExpandArgs;
use crate::cli::config::{SecretVars, expand_config_for_display};
use crate::cli::{CliError, ExitCode};

/// Print the config with `${VAR}` expressions expanded.
pub(crate) fn run(args: &ExpandArgs) -> Result<ExitCode, CliError> {
    print!("{}", render(args)?);
    Ok(ExitCode::Success)
}

/// The expanded config text; secret-pattern variables are rejected unless
/// `--show-secrets` is set.
fn render(args: &ExpandArgs) -> Result<String, CliError> {
    let secrets = if args.show_secrets {
        SecretVars::Allow
    } else {
        SecretVars::Block
    };
    Ok(expand_config_for_display(&args.config, secrets)?)
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::ExpandArgs;
    use crate::cli::CliError;
    use crate::cli::config::{ConfigError, SecretVars, expand_config_for_display};
    use crate::cli::test_support::{lock_env, write_temp_config};

    #[test]
    fn expand_resolves_env_vars() {
        let file = write_temp_config(
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
        let _env = lock_env();
        // SAFETY: `lock_env` serializes the tests that set or remove variables.
        unsafe {
            std::env::set_var(
                "OCYNC_TEST_ECR_URL",
                "123456.dkr.ecr.us-east-1.amazonaws.com",
            )
        };
        let expanded = expand_config_for_display(file.path(), SecretVars::Allow).unwrap();
        assert!(expanded.contains("123456.dkr.ecr.us-east-1.amazonaws.com"));
        assert!(!expanded.contains("OCYNC_TEST_ECR_URL"));
        unsafe { std::env::remove_var("OCYNC_TEST_ECR_URL") };
    }

    #[test]
    fn show_secrets_flag_controls_secret_variables() {
        let file = write_temp_config("token: \"${OCYNC_EXPAND_FLAG_SECRET}\"\n");
        let _env = lock_env();
        // SAFETY: `lock_env` serializes the tests that set or remove variables.
        unsafe { std::env::set_var("OCYNC_EXPAND_FLAG_SECRET", "s3cret") };
        // `Err` carries the blocked variable's name.
        let cases = [
            ("flag off blocks", false, Err("OCYNC_EXPAND_FLAG_SECRET")),
            ("flag on shows", true, Ok("token: \"s3cret\"\n")),
        ];
        for (name, show_secrets, want) in cases {
            let got = render(&ExpandArgs {
                config: file.path().to_path_buf(),
                show_secrets,
            });
            match (got, want) {
                (Ok(text), Ok(want)) => assert_eq!(text, want, "{name}"),
                (Err(CliError::Config(ConfigError::BlockedEnvVar(var))), Err(want)) => {
                    assert_eq!(var, want, "{name}");
                }
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
        unsafe { std::env::remove_var("OCYNC_EXPAND_FLAG_SECRET") };
    }

    #[test]
    fn expand_nonexistent_file() {
        let dir = tempfile::tempdir().expect("tempdir creates");
        let result = expand_config_for_display(&dir.path().join("missing.yaml"), SecretVars::Block);
        assert!(result.is_err());
    }
}
