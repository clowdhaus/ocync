//! The process environment, file system, platform, clock and network the
//! AWS settings and credentials come from, behind enums so tests can supply
//! fixed values and recorded HTTP exchanges.

#[cfg(test)]
use std::collections::HashMap;
use std::env::VarError;
use std::io;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Arc;
use std::time::SystemTime;

#[cfg(test)]
use super::replay::Replay;

/// Everything the settings and credentials are read from.
#[derive(Debug, Clone)]
pub(super) struct Host {
    pub(super) env: Env,
    pub(super) fs: Fs,
    pub(super) platform: Platform,
    pub(super) clock: Clock,
    pub(super) network: Network,
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
    /// A fixed set of files. A path not in the map is not found.
    #[cfg(test)]
    Fixed(HashMap<PathBuf, Vec<u8>>),
}

impl Fs {
    /// The whole contents of `path`. The files read here are a few
    /// kilobytes on local disk, so a blocking read is used.
    pub(super) fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match self {
            Self::Real => std::fs::read(path),
            #[cfg(test)]
            Self::Fixed(files) => files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound)),
        }
    }
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
