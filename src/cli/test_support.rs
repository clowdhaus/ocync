//! Fixtures shared by tests across the `cli` modules: the config file on disk,
//! the process-wide environment lock, a placeholder client and a resolved
//! mapping.

use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard};

use ocync_distribution::{RegistryClient, RegistryClientBuilder};

/// Serializes the tests that set or remove process environment variables
/// against each other. A test that only reads a variable takes no lock, so
/// each variable a writer sets is read only by tests that also hold the lock
/// or that do not depend on its value. Take it through [`lock_env`].
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// Hold the returned guard for the whole of a test that sets or removes an
/// environment variable. A panic in an earlier holder does not matter: each test sets
/// the variables it reads.
pub(crate) fn lock_env() -> MutexGuard<'static, ()> {
    ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

/// A config file holding `content`, deleted when the returned handle drops.
pub(crate) fn write_temp_config(content: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("temp config creates");
    file.write_all(content.as_bytes())
        .expect("temp config writes");
    file
}

/// An anonymous client pointed at a mock server.
pub(crate) fn test_client(url: &str) -> Arc<RegistryClient> {
    Arc::new(
        RegistryClientBuilder::new(url::Url::parse(url).expect("mock server url parses"))
            .build()
            .expect("client builds"),
    )
}

/// A single-target mapping over `tags` carrying `filter_report`, for
/// formatter and wiring tests. The clients are placeholders that issue no
/// requests.
pub(crate) fn mapping_with_tags_and_report(
    tags: Vec<ocync_sync::engine::TagPair>,
    filter_report: Option<ocync_sync::filter::FilterReport>,
) -> ocync_sync::engine::ResolvedMapping {
    use ocync_distribution::spec::{RegistryAuthority, RepositoryName};
    use ocync_sync::engine::{RegistryAlias, ResolvedArtifacts, ResolvedMapping, TargetEntry};
    use std::rc::Rc;

    let client = test_client("http://127.0.0.1");

    ResolvedMapping {
        source_authority: RegistryAuthority::new("source.test:443"),
        source_client: client.clone(),
        source_repo: RepositoryName::new("repo").unwrap(),
        target_repo: RepositoryName::new("repo").unwrap(),
        targets: vec![TargetEntry::new(RegistryAlias::new("target"), client, None).unwrap()],
        tags,
        platforms: None,
        head_first: false,
        immutable_glob: None,
        artifacts_config: Rc::new(ResolvedArtifacts::default()),
        candidate_count: filter_report.as_ref().map(|r| r.candidate_count),
        filter_report,
    }
}
