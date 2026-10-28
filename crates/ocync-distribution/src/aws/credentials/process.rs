//! Credentials from the command a profile's `credential_process` names,
//! run and read as upstream runs and reads it.

use std::fmt;
use std::io;
use std::process::ExitStatus;
use std::str::Utf8Error;

use serde_json::Value;

use super::super::calendar;
use super::{Credentials, json};
use crate::error::truncate_quoted;

/// The only document version upstream reads.
const VERSION: i32 = 1;

/// A `credential_process` command. It prints only its first word, as
/// upstream does: the arguments may carry secrets.
#[derive(Clone, Copy)]
pub(super) struct Command<'a>(&'a str);

impl<'a> Command<'a> {
    /// The command line `text`.
    pub(super) fn new(text: &'a str) -> Self {
        Self(text)
    }

    /// The whole command line, arguments included, to run.
    pub(super) fn unredacted(self) -> &'a str {
        self.0
    }
}

impl fmt::Display for Command<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.split_once(char::is_whitespace) {
            Some((program, _)) => write!(f, "{program} ** arguments redacted **"),
            None => f.write_str(self.0),
        }
    }
}

impl fmt::Debug for Command<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_string())
    }
}

/// Why the command gave no credentials. The texts are upstream's.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Error {
    /// The shell did not start.
    #[error("Error retrieving credentials from external process")]
    Spawn(#[source] io::Error),
    /// The command failed.
    #[error(
        "Error retrieving credentials: external process exited with code {status}. Stderr: {stderr}"
    )]
    Exited { status: ExitStatus, stderr: String },
    /// The output is not UTF-8.
    #[error("Error retrieving credentials from external process: could not decode output as UTF-8")]
    NotUtf8(#[source] Utf8Error),
    /// The output is not a credentials document.
    #[error("Error retrieving credentials from external process, could not parse response")]
    Invalid(#[source] Invalid),
}

/// What is wrong with the document the command printed.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Invalid {
    /// Not a JSON object.
    #[error("invalid JSON in response")]
    Json(#[source] serde_json::Error),
    /// A required field is missing or not of its type.
    #[error("Expected field `{0}` in response but it was missing")]
    Missing(&'static str),
    /// `Version` is a number that is not a 32-bit integer.
    #[error("Invalid field in response: `Version`. not a 32-bit integer")]
    VersionNotInteger,
    /// `Version` is not the one version there is.
    #[error("Invalid field in response: `version`. unknown version number: {0}")]
    Version(i32),
    /// `Expiration` is not an RFC 3339 time.
    #[error("Invalid field in response: `Expiration`. '{0}' is not an RFC 3339 time")]
    Expiration(String),
}

/// Run `command` through the shell and read the credentials it prints. The
/// process is killed if the load is dropped, as when the chain times out.
pub(super) async fn load(command: Command<'_>) -> Result<Credentials, Error> {
    tracing::debug!(%command, "loading AWS credentials from an external process");
    let output = shell(command.unredacted())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(Error::Spawn)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Exited {
            status: output.status,
            stderr: truncate_quoted(&stderr).to_owned(),
        });
    }
    let stdout = std::str::from_utf8(&output.stdout).map_err(Error::NotUtf8)?;
    parse(stdout).map_err(Error::Invalid)
}

/// `sh -c <command>`, as upstream runs it off Windows.
#[cfg(not(windows))]
fn shell(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("sh");
    shell.args(["-c", command]);
    shell
}

/// `cmd.exe /C "<command>"`, the command passed as written, as upstream
/// runs it on Windows.
#[cfg(windows)]
fn shell(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("cmd.exe");
    shell.arg("/C").raw_arg(format!("\"{command}\""));
    shell
}

/// The credentials in the document `output` holds, its fields found as
/// [`json::field`] finds them: a field of another JSON type counts as
/// missing.
fn parse(output: &str) -> Result<Credentials, Invalid> {
    let document: serde_json::Map<String, Value> =
        serde_json::from_str(output).map_err(Invalid::Json)?;
    let text = |name: &'static str| json::field(&document, &[name], Value::as_str);
    let version = json::field(&document, &["Version"], Value::as_number)
        .ok_or(Invalid::Missing("Version"))?;
    match version.as_i64().and_then(|n| i32::try_from(n).ok()) {
        Some(VERSION) => {}
        Some(other) => return Err(Invalid::Version(other)),
        None => return Err(Invalid::VersionNotInteger),
    }
    let required = |name: &'static str| text(name).map(str::to_owned).ok_or(Invalid::Missing(name));
    let expiry = text("Expiration")
        .map(|expiration| {
            calendar::parse_rfc3339_with_offset(expiration)
                .ok_or_else(|| Invalid::Expiration(expiration.to_owned()))
        })
        .transpose()?;
    Ok(Credentials {
        access_key_id: required("AccessKeyId")?,
        secret_access_key: required("SecretAccessKey")?,
        session_token: text("SessionToken").map(str::to_owned),
        expiry,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::error::error_chain;
    use crate::test_logs::capture_logs;

    /// The command prints only its program, so arguments that carry a
    /// secret stay out of logs and errors. Breaks if an argument prints.
    #[test]
    fn prints_only_the_program() {
        assert_eq!(
            Command("get-creds --token s3cret").to_string(),
            "get-creds ** arguments redacted **"
        );
        assert_eq!(Command("get-creds").to_string(), "get-creds");
        assert_eq!(
            format!("{:?}", Command("get-creds\t--token s3cret")),
            "\"get-creds ** arguments redacted **\""
        );
    }

    /// The document is read as upstream reads it: keys in any case, an
    /// offset expiry, no session token or expiry required, and each wrong
    /// document refused with upstream's reason. Breaks if a key matches
    /// only in one case, a missing field reads as empty, or another
    /// version is taken.
    #[test]
    fn reads_the_document_as_upstream_does() {
        let loaded = parse(
            r#"{"version": 1, "accesskeyid": "AKID", "SecretAccessKey": "secret",
                "SessionToken": "token", "Expiration": "2022-05-02T20:36:00+02:00"}"#,
        )
        .unwrap();
        assert_eq!(loaded.access_key_id(), "AKID");
        assert_eq!(loaded.secret_access_key(), "secret");
        assert_eq!(loaded.session_token(), Some("token"));
        assert_eq!(
            loaded.expiry(),
            Some(UNIX_EPOCH + Duration::from_secs(1_651_516_560))
        );
        for document in [
            r#"{"Version": 1, "AccessKeyId": 7, "accesskeyid": "AKID", "SecretAccessKey": "s"}"#,
            r#"{"Version": 1, "AccessKeyId": "AKID", "accesskeyid": 7, "SecretAccessKey": "s"}"#,
        ] {
            assert_eq!(
                parse(document).unwrap().access_key_id(),
                "AKID",
                "{document}"
            );
        }
        let lasting =
            parse(r#"{"Version": 1, "AccessKeyId": "AKID", "SecretAccessKey": "s"}"#).unwrap();
        assert_eq!((lasting.session_token(), lasting.expiry()), (None, None));
        for (document, error) in [
            ("invalid json", "invalid JSON in response"),
            (
                r#"{"AccessKeyId": "AKID", "SecretAccessKey": "s"}"#,
                "Expected field `Version` in response but it was missing",
            ),
            (
                r#"{"Version": "1", "AccessKeyId": "AKID", "SecretAccessKey": "s"}"#,
                "Expected field `Version` in response but it was missing",
            ),
            (
                r#"{"Version": 2, "AccessKeyId": "AKID", "SecretAccessKey": "s"}"#,
                "Invalid field in response: `version`. unknown version number: 2",
            ),
            (
                r#"{"Version": 1.5, "AccessKeyId": "AKID", "SecretAccessKey": "s"}"#,
                "Invalid field in response: `Version`. not a 32-bit integer",
            ),
            (
                r#"{"Version": 1, "AccessKeyId": 7, "SecretAccessKey": "s"}"#,
                "Expected field `AccessKeyId` in response but it was missing",
            ),
            (
                r#"{"Version": 1, "AccessKeyId": "AKID"}"#,
                "Expected field `SecretAccessKey` in response but it was missing",
            ),
            (
                r#"{"Version": 1, "AccessKeyId": "AKID", "SecretAccessKey": "s",
                    "Expiration": "tomorrow"}"#,
                "Invalid field in response: `Expiration`. 'tomorrow' is not an RFC 3339 time",
            ),
        ] {
            let message = parse(document).unwrap_err().to_string();
            assert!(message.starts_with(error), "{document}: {message}");
        }
    }

    /// A command that fails is reported with its status and what it wrote
    /// to stderr, and the command's arguments stay out of the logs. Breaks
    /// if a failing command is read as empty credentials or an argument is
    /// logged.
    #[cfg(unix)]
    #[tokio::test]
    async fn reports_a_failing_command_without_its_arguments() {
        let (result, logs) = capture_logs(load(Command("echo the-argument >&2; exit 3"))).await;
        let error = result.unwrap_err();
        assert_eq!(
            error_chain(&error),
            "Error retrieving credentials: external process exited with code \
             exit status: 3. Stderr: the-argument\n"
        );
        assert!(logs.contains("echo ** arguments redacted **"), "{logs}");
        assert!(!logs.contains("the-argument"), "{logs}");
    }

    /// A failing command's stderr is cut to its start, as it can hold a
    /// token and the error reaches the report, and a byte that is not UTF-8
    /// past the cut costs nothing. Breaks if all of it is kept, or the text
    /// is replaced for that byte.
    #[cfg(unix)]
    #[tokio::test]
    async fn keeps_only_the_start_of_a_failing_commands_stderr() {
        let error = load(Command(
            "printf 'a%.0s' $(seq 300) >&2; printf '\\377' >&2; exit 1",
        ))
        .await
        .unwrap_err();
        let Error::Exited { stderr, .. } = error else {
            panic!("{error}");
        };
        assert_eq!(stderr, "a".repeat(200));
    }

    /// A command still running when its load is dropped is killed, so a
    /// hung helper does not outlive the chain's timeout. Breaks if the
    /// process is left running.
    #[cfg(unix)]
    #[tokio::test]
    async fn kills_a_command_whose_load_is_dropped() {
        let marker = std::env::temp_dir().join(format!("ocync-process-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let command = format!("sleep 1; touch '{}'", marker.display());
        let load = load(Command(&command));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), load)
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "the command ran on after its load was dropped"
        );
    }
}
