//! The `expand` subcommand - shows resolved config with env var expansion.

use crate::ExpandArgs;
use crate::cli::config::expand_config_for_display;
use crate::cli::{CliError, ExitCode};

pub(crate) fn run(args: &ExpandArgs) -> Result<ExitCode, CliError> {
    let yaml = expand_config_for_display(&args.config, args.show_secrets)?;
    print!("{yaml}");
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::cli::config::expand_config_for_display;

    fn write_temp_config(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("ocync_test_{name}.yaml"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

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
        let expanded = expand_config_for_display(&path, true).unwrap();
        assert!(expanded.contains("123456.dkr.ecr.us-east-1.amazonaws.com"));
        assert!(!expanded.contains("OCYNC_TEST_ECR_URL"));
        unsafe { std::env::remove_var("OCYNC_TEST_ECR_URL") };
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expand_blocks_secret_vars_without_show_secrets() {
        let path = write_temp_config(
            "expand_blocked",
            r#"
registries:
  hub:
    url: "${OCYNC_EXPAND_SECRET_BLOCKED}"
mappings:
  - from: nginx
    tags:
      glob: "*"
"#,
        );
        unsafe { std::env::set_var("OCYNC_EXPAND_SECRET_BLOCKED", "s3cret") };
        let result = expand_config_for_display(&path, false);
        assert!(result.is_err());
        unsafe { std::env::remove_var("OCYNC_EXPAND_SECRET_BLOCKED") };
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expand_allows_secret_vars_with_show_secrets() {
        let path = write_temp_config(
            "expand_allowed",
            r#"
registries:
  hub:
    url: "${OCYNC_EXPAND_SECRET_ALLOWED}"
mappings:
  - from: nginx
    tags:
      glob: "*"
"#,
        );
        unsafe { std::env::set_var("OCYNC_EXPAND_SECRET_ALLOWED", "s3cret") };
        let expanded = expand_config_for_display(&path, true).unwrap();
        assert!(expanded.contains("s3cret"));
        unsafe { std::env::remove_var("OCYNC_EXPAND_SECRET_ALLOWED") };
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expand_nonexistent_file() {
        let result =
            expand_config_for_display(std::path::Path::new("/tmp/ocync_test_missing.yaml"), false);
        assert!(result.is_err());
    }
}
