//! The process environment and file system the AWS credential chain reads,
//! behind enums so tests can supply fixed values instead.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// Where environment variables come from.
#[derive(Debug, Clone)]
pub(super) enum Env {
    /// The running process's environment.
    Process,
    /// A fixed set, for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(HashMap<String, String>),
}

impl Env {
    /// The value of `key`, or `None` when unset or not UTF-8.
    pub(super) fn get(&self, key: &str) -> Option<String> {
        match self {
            Self::Process => std::env::var(key).ok(),
            Self::Fixed(vars) => vars.get(key).cloned(),
        }
    }
}

/// Where files come from.
#[derive(Debug, Clone)]
pub(super) enum Fs {
    /// The real file system.
    Real,
    /// A fixed set of files, for tests. A path not in the map is not found.
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(HashMap<PathBuf, Vec<u8>>),
}

impl Fs {
    /// The whole contents of `path`. The files read here are a few
    /// kilobytes on local disk, so a blocking read is used.
    pub(super) fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match self {
            Self::Real => std::fs::read(path),
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
