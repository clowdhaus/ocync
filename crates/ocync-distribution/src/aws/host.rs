//! The process environment, file system, platform, clock, network and DNS the
//! AWS settings and credentials come from, behind enums so tests can supply
//! fixed values and recorded HTTP exchanges, and the SSO token refresh state
//! a process shares.

#[cfg(test)]
use std::collections::HashMap;
use std::env::VarError;
use std::io::{self, Write as _};
use std::net::IpAddr;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::credentials::Sessions;
#[cfg(test)]
use super::replay::Replay;

/// Everything the settings and credentials are read from, and the SSO
/// token refreshes the chains on it share.
#[derive(Debug, Clone)]
pub(super) struct Host {
    pub(super) env: Env,
    pub(super) fs: Fs,
    pub(super) platform: Platform,
    pub(super) clock: Clock,
    pub(super) network: Network,
    pub(super) dns: Dns,
    pub(super) sso_sessions: Sessions,
}

impl Host {
    /// This process on the platform it runs on.
    pub(super) fn current() -> Self {
        Self {
            env: Env::Process,
            fs: Fs::Real,
            platform: Platform::current(),
            clock: Clock::System,
            network: Network::Real,
            dns: Dns::System,
            sso_sessions: Sessions::process(),
        }
    }
}

#[cfg(test)]
impl Host {
    /// A Unix host on the system clock with no variables, files or names,
    /// whose network fails the test on any request, sharing SSO token
    /// refreshes with no other host. Tests override only what they set.
    pub(super) fn fixed() -> Self {
        Self {
            env: Env::Fixed(HashMap::new()),
            fs: Fs::Fixed(HashMap::new()),
            platform: Platform::Unix,
            clock: Clock::System,
            network: Network::Replay(Arc::new(Replay::of(std::iter::empty()))),
            dns: Dns::Fixed(HashMap::new()),
            sso_sessions: Sessions::default(),
        }
    }
}

/// Where host names are resolved.
#[derive(Debug, Clone)]
pub(super) enum Dns {
    /// The system resolver.
    System,
    /// A fixed set of names. A name not in the map does not resolve.
    #[cfg(test)]
    Fixed(HashMap<String, Vec<IpAddr>>),
}

impl Dns {
    /// The addresses `name` resolves to.
    pub(super) async fn resolve(&self, name: &str) -> io::Result<Vec<IpAddr>> {
        match self {
            Self::System => Ok(tokio::net::lookup_host((name, 0))
                .await?
                .map(|address| address.ip())
                .collect()),
            #[cfg(test)]
            Self::Fixed(names) => names
                .get(name)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound)),
        }
    }
}

/// Where the time comes from.
#[derive(Debug, Clone, Copy)]
pub(super) enum Clock {
    /// The system clock.
    System,
    /// A fixed instant.
    #[cfg(test)]
    Fixed(SystemTime),
}

impl Clock {
    /// The time now.
    pub(super) fn now(self) -> SystemTime {
        match self {
            Self::System => SystemTime::now(),
            #[cfg(test)]
            Self::Fixed(time) => time,
        }
    }
}

/// Where HTTP requests go.
#[derive(Debug, Clone)]
pub(super) enum Network {
    /// The network, through a client each source configures.
    Real,
    /// Recorded exchanges, answered in order.
    #[cfg(test)]
    Replay(Arc<Replay>),
}

impl Network {
    /// The transport for a source whose client `builder` configures.
    pub(super) fn transport(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> Result<Transport, reqwest::Error> {
        match self {
            Self::Real => {
                crate::install_crypto_provider();
                builder.build().map(Transport::Client)
            }
            #[cfg(test)]
            Self::Replay(replay) => Ok(Transport::Replay(Arc::clone(replay))),
        }
    }
}

/// How long a connection and each read may take.
#[derive(Debug, Clone, Copy)]
pub(super) struct Timeouts {
    pub(super) connect: Duration,
    pub(super) read: Duration,
}

/// The client settings for a credential endpoint the token is meant for
/// alone, the instance metadata service or the container endpoint: no
/// proxy, so a token never reaches an `HTTP_PROXY` host, and no redirect.
pub(super) fn credential_endpoint_client(timeouts: Timeouts) -> reqwest::ClientBuilder {
    crate::client::no_redirect_builder()
        .no_proxy()
        .connect_timeout(timeouts.connect)
        .read_timeout(timeouts.read)
}

/// Sends a source's requests.
#[derive(Debug, Clone)]
pub(super) enum Transport {
    /// Over the network.
    Client(reqwest::Client),
    /// To recorded exchanges.
    #[cfg(test)]
    Replay(Arc<Replay>),
}

impl Transport {
    /// Send `request` and return the answer's head; the body streams.
    pub(super) async fn send(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, reqwest::Error> {
        match self {
            Self::Client(client) => client.execute(request).await,
            #[cfg(test)]
            Self::Replay(replay) => Ok(replay.answer(&request).await),
        }
    }
}

/// Where environment variables come from.
#[derive(Debug, Clone)]
pub(super) enum Env {
    /// The running process's environment.
    Process,
    /// A fixed set.
    #[cfg(test)]
    Fixed(HashMap<String, String>),
}

impl Env {
    /// The value of `key`, or `None` when unset. A value that is not UTF-8
    /// is also `None`, with a warning, as the SDK treats it as unset.
    pub(super) fn get(&self, key: &str) -> Option<String> {
        match self {
            Self::Process => match std::env::var(key) {
                Ok(value) => Some(value),
                Err(VarError::NotPresent) => None,
                Err(VarError::NotUnicode(_)) => {
                    tracing::warn!(variable = key, "environment variable ignored, not UTF-8");
                    None
                }
            },
            #[cfg(test)]
            Self::Fixed(vars) => vars.get(key).cloned(),
        }
    }
}

/// Where files come from.
#[derive(Debug, Clone)]
pub(super) enum Fs {
    /// The real file system.
    Real,
    /// A fixed set of files. A path not in the map is not found, as the
    /// operating system reports it.
    #[cfg(test)]
    Fixed(HashMap<PathBuf, Vec<u8>>),
}

/// The `errno` of a missing file on Linux and macOS, where the tests run.
#[cfg(test)]
const ENOENT: i32 = 2;

/// The mode of a file only its owner may read or write.
#[cfg(unix)]
const OWNER_ONLY: u32 = 0o600;

impl Fs {
    /// Replace `path` with `contents`, readable and writable by its owner
    /// alone where the platform has modes: written to a new file beside it,
    /// then renamed over it, so a reader sees the old file or the new, never
    /// part of one. Not flushed to disk, so the call does not wait on it;
    /// the files written here are caches their owner can rebuild. Blocking,
    /// as [`Self::read`] is.
    pub(super) fn replace_private(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        match self {
            Self::Real => {
                let mut name = path
                    .file_name()
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
                    .to_owned();
                name.push(format!(".{}.tmp", std::process::id()));
                let temp = path.with_file_name(name);
                let replaced =
                    write_new_private(&temp, contents).and_then(|()| std::fs::rename(&temp, path));
                if replaced.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                replaced
            }
            #[cfg(test)]
            Self::Fixed(_) => Err(io::Error::from(io::ErrorKind::ReadOnlyFilesystem)),
        }
    }
    /// The whole contents of `path`. The files read here are a few
    /// kilobytes on local disk, so a blocking read is used.
    pub(super) fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match self {
            Self::Real => std::fs::read(path),
            #[cfg(test)]
            Self::Fixed(files) => files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from_raw_os_error(ENOENT)),
        }
    }
}

/// Write `contents` to `path` as a new file only its owner may read. A file
/// left at `path` by a write this process did not finish is removed first.
fn write_new_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, OWNER_ONLY);
    options.open(path)?.write_all(contents)
}

/// The platform rules for finding the home directory and joining paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Platform {
    /// Linux, macOS and other Unix-like systems.
    Unix,
    /// Windows.
    Windows,
}

impl Platform {
    /// The platform this binary runs on.
    pub(super) fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }

    /// The path separator.
    pub(super) fn separator(self) -> char {
        match self {
            Self::Unix => '/',
            Self::Windows => '\\',
        }
    }

    /// The home directory: `HOME`, then on Windows `USERPROFILE`, then
    /// `HOMEDRIVE` followed by `HOMEPATH`.
    pub(super) fn home_dir(self, env: &Env) -> Option<String> {
        if let Some(home) = env.get("HOME") {
            return Some(home);
        }
        match self {
            Self::Unix => None,
            Self::Windows => env.get("USERPROFILE").or_else(|| {
                let drive = env.get("HOMEDRIVE")?;
                let path = env.get("HOMEPATH")?;
                Some(drive + &path)
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A replaced file holds the new contents alone, readable by its owner
    /// alone, with no temporary file left beside it, even when a stale one
    /// is in the way. Breaks if the file is written in place, keeps the
    /// old mode, or a temporary file is left.
    #[cfg(unix)]
    #[test]
    fn replaces_a_file_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("ocync-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token.json");
        std::fs::write(&path, "old contents, longer than the new").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(
            dir.join(format!("token.json.{}.tmp", std::process::id())),
            "stale",
        )
        .unwrap();

        Fs::Real.replace_private(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["token.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
