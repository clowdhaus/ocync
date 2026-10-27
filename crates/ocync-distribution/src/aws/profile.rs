//! The shared AWS config and credentials files: where they are, how they
//! parse and how the two merge, as the AWS SDKs and CLI read them.
//!
//! Both are INI-like. A `[section]` line opens a section, `key = value`
//! lines set properties, an indented line continues the previous value, and
//! a property with an empty value holds indented `key = value` sub-properties.
//! The config file names profiles `[profile name]` (only `default` may go
//! unprefixed) and also holds `[sso-session name]` and `[services name]`
//! sections; the credentials file names profiles bare. The credentials file
//! is merged second, so its values win.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use super::host::{Env, Fs, Platform};

/// Characters that count as whitespace in these files.
const WHITESPACE: &[char] = &[' ', '\t'];

/// Characters that start a comment.
const COMMENT: &[char] = &['#', ';'];

/// The profile used when none is named.
pub(super) const DEFAULT_PROFILE: &str = "default";

/// The prefix of a profile section in the config file.
const PROFILE_PREFIX: &str = "profile";

/// The prefix of an IAM Identity Center session section.
const SSO_SESSION_PREFIX: &str = "sso-session";

/// The prefix of a section holding per-service settings.
const SERVICES_PREFIX: &str = "services";

/// One section's properties, keyed by lowercased name.
pub(super) type Properties = HashMap<String, String>;

/// Which of the two files a body of text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    /// The config file, `~/.aws/config`.
    Config,
    /// The credentials file, `~/.aws/credentials`.
    Credentials,
}

impl FileKind {
    /// The environment variable that overrides this file's location.
    fn override_var(self) -> &'static str {
        match self {
            Self::Config => "AWS_CONFIG_FILE",
            Self::Credentials => "AWS_SHARED_CREDENTIALS_FILE",
        }
    }

    /// The location when not overridden.
    fn default_path(self) -> &'static str {
        match self {
            Self::Config => "~/.aws/config",
            Self::Credentials => "~/.aws/credentials",
        }
    }
}

/// A `[services name]` section: each service's settings, keyed by the
/// service's profile spelling, such as `ecr-public`.
pub(super) type Services = HashMap<String, Properties>;

/// Everything read from the two files.
#[derive(Debug, Default)]
pub(super) struct Sections {
    /// Profiles by name.
    pub(super) profiles: HashMap<String, Properties>,
    /// `[sso-session name]` sections by name.
    pub(super) sso_sessions: HashMap<String, Properties>,
    /// `[services name]` sections by name.
    pub(super) services: HashMap<String, Services>,
}

/// A file that does not parse.
#[derive(Debug, Clone, thiserror::Error)]
#[error("error parsing {} on line {line}: {message}", path.display())]
pub(super) struct ParseError {
    path: PathBuf,
    line: usize,
    message: String,
}

/// Who chose a file's location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The default under the home directory.
    Default,
    /// An environment variable.
    Env,
}

/// One of the two files and who chose its location.
#[derive(Debug, PartialEq, Eq)]
struct File {
    path: PathBuf,
    origin: Origin,
}

/// Where the two files are and which profile is selected.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Location {
    config: File,
    credentials: File,
    /// `AWS_PROFILE`, or `default`.
    pub(super) profile: String,
}

impl Location {
    /// Finds the files from `AWS_CONFIG_FILE`, `AWS_SHARED_CREDENTIALS_FILE`
    /// and the home directory, expanding a leading `~`.
    pub(super) fn find(env: &Env, platform: Platform) -> Self {
        let home = platform.home_dir(env);
        let path = |kind: FileKind| {
            let (path, origin) = match env.get(kind.override_var()) {
                Some(path) => (path, Origin::Env),
                None => (kind.default_path().to_owned(), Origin::Default),
            };
            File {
                path: PathBuf::from(expand_home(&path, home.as_deref(), platform)),
                origin,
            }
        };
        Self {
            config: path(FileKind::Config),
            credentials: path(FileKind::Credentials),
            profile: env
                .get("AWS_PROFILE")
                .unwrap_or_else(|| DEFAULT_PROFILE.to_owned()),
        }
    }
}

/// Replaces a leading `~` path component with the home directory, joining
/// the rest with the platform's separator. Left as is without a home.
pub(super) fn expand_home(path: &str, home: Option<&str>, platform: Platform) -> String {
    let is_separator = |c: char| c == '/' || c == platform.separator();
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_owned();
    };
    if !(rest.is_empty() || rest.starts_with(is_separator)) {
        return path.to_owned();
    }
    let Some(home) = home else {
        tracing::warn!(
            path,
            "no home directory to expand `~` in an AWS config path"
        );
        return path.to_owned();
    };
    let sep = platform.separator();
    let mut expanded = home.to_owned();
    for component in rest.split(is_separator).filter(|c| !c.is_empty()) {
        if !expanded.ends_with(is_separator) {
            expanded.push(sep);
        }
        expanded.push_str(component);
    }
    expanded
}

/// Reads and merges the config and credentials files. A file that cannot
/// be read or is not UTF-8 counts as empty, with a warning unless it is a
/// default location that does not exist.
pub(super) fn load(fs: &Fs, location: &Location) -> Result<Sections, ParseError> {
    let mut sections = Sections::default();
    for (kind, file) in [
        (FileKind::Config, &location.config),
        (FileKind::Credentials, &location.credentials),
    ] {
        let path = &file.path;
        let contents = match fs.read(path) {
            Ok(bytes) => String::from_utf8(bytes).unwrap_or_else(|_| {
                tracing::warn!(path = %path.display(), "AWS config file is not UTF-8, ignored");
                String::new()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if file.origin == Origin::Env {
                    tracing::warn!(
                        path = %path.display(),
                        var = kind.override_var(),
                        "AWS config file named by the environment not found"
                    );
                }
                String::new()
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "AWS config file not read");
                String::new()
            }
        };
        merge(&mut sections, &contents, kind, path)?;
    }
    Ok(sections)
}

/// A section as written, before its name is checked: its header text and
/// raw properties.
type RawSection<'a> = (&'a str, Vec<(String, String)>);

/// Parses `contents` as a file of `kind` and merges it into `sections`.
fn merge(
    sections: &mut Sections,
    contents: &str,
    kind: FileKind,
    path: &Path,
) -> Result<(), ParseError> {
    let raw = parse(contents, path)?;
    let valid: Vec<_> = raw
        .into_iter()
        .filter_map(
            |(header, properties)| match SectionName::parse(header).valid_for(kind) {
                Ok(name) => Some((name, properties)),
                Err(reason) => {
                    tracing::warn!("AWS config section ignored: {reason}");
                    None
                }
            },
        )
        .collect();
    let has_prefixed_default = valid.iter().any(|(name, _)| name.is_prefixed_default());
    for (name, properties) in valid {
        if has_prefixed_default && name.is_unprefixed_default() {
            tracing::warn!("AWS config section [default] ignored for [profile default]");
            continue;
        }
        let target = match name.kind {
            SectionKind::Bare | SectionKind::Profile => {
                sections.profiles.entry(name.suffix.to_owned())
            }
            SectionKind::SsoSession => sections.sso_sessions.entry(name.suffix.to_owned()),
            SectionKind::Services => {
                let services = sections.services.entry(name.suffix.to_owned()).or_default();
                merge_services(services, name.suffix, properties);
                continue;
            }
            SectionKind::Other(prefix) => {
                tracing::debug!(prefix, section = name.suffix, "AWS config section not read");
                continue;
            }
        }
        .or_default();
        for (key, value) in properties {
            if is_identifier(&key) {
                target.insert(key, value);
            } else {
                tracing::warn!(
                    section = name.suffix,
                    key,
                    "AWS config key ignored, not a valid name"
                );
            }
        }
    }
    Ok(())
}

/// Stores each service's sub-properties from the `[services section]`
/// section, such as `endpoint_url` under `ecr`.
fn merge_services(services: &mut Services, section: &str, properties: Vec<(String, String)>) {
    for (service, value) in properties {
        if !is_identifier(&service) {
            tracing::warn!(section, service, "AWS config key ignored, not a valid name");
            continue;
        }
        let settings = services.entry(service).or_default();
        for line in value.split('\n').filter(|l| !l.is_empty()) {
            let Some((key, value)) = line.split_once('=') else {
                tracing::warn!(line, "AWS config sub-property ignored, no '='");
                continue;
            };
            settings.insert(
                key.trim_matches(WHITESPACE).to_owned(),
                value.trim_matches(WHITESPACE).to_owned(),
            );
        }
    }
}

/// Whether `s` may name a profile, section or key.
fn is_identifier(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "_-/.%@:+".contains(c))
}

/// What a section header's prefix makes the section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionKind<'a> {
    /// No prefix: `[default]`, or any credentials file section.
    Bare,
    /// `[profile name]`.
    Profile,
    /// `[sso-session name]`.
    SsoSession,
    /// `[services name]`.
    Services,
    /// A prefix that is not read.
    Other(&'a str),
}

impl<'a> SectionKind<'a> {
    fn parse(prefix: Option<&'a str>) -> Self {
        match prefix {
            None => Self::Bare,
            Some(PROFILE_PREFIX) => Self::Profile,
            Some(SSO_SESSION_PREFIX) => Self::SsoSession,
            Some(SERVICES_PREFIX) => Self::Services,
            Some(other) => Self::Other(other),
        }
    }

    /// The prefix as written, if any.
    fn prefix(self) -> Option<&'a str> {
        match self {
            Self::Bare => None,
            Self::Profile => Some(PROFILE_PREFIX),
            Self::SsoSession => Some(SSO_SESSION_PREFIX),
            Self::Services => Some(SERVICES_PREFIX),
            Self::Other(prefix) => Some(prefix),
        }
    }
}

/// A section header split into its kind and its name.
struct SectionName<'a> {
    kind: SectionKind<'a>,
    suffix: &'a str,
}

impl<'a> SectionName<'a> {
    fn parse(header: &'a str) -> Self {
        let header = header.trim_matches(WHITESPACE);
        let (prefix, suffix) = match header.split_once(WHITESPACE) {
            Some((prefix, suffix)) => (Some(prefix.trim()), suffix.trim()),
            None => (None, header.trim()),
        };
        Self {
            kind: SectionKind::parse(prefix),
            suffix,
        }
    }

    fn is_unprefixed_default(&self) -> bool {
        self.kind == SectionKind::Bare && self.suffix == DEFAULT_PROFILE
    }

    fn is_prefixed_default(&self) -> bool {
        self.kind == SectionKind::Profile && self.suffix == DEFAULT_PROFILE
    }

    /// The config file needs a prefix on every section but `[default]`; the
    /// credentials file allows no prefix at all.
    fn valid_for(self, kind: FileKind) -> Result<Self, String> {
        let suffix = self.suffix;
        match (kind, self.kind.prefix()) {
            (FileKind::Config, Some(prefix)) if !is_identifier(suffix) => Err(format!(
                "[{prefix} {suffix}]: `{suffix}` is not a valid name"
            )),
            (FileKind::Config, None) if !self.is_unprefixed_default() => Err(format!(
                "[{suffix}]: config file sections other than [default] need a prefix, as in [profile {suffix}]"
            )),
            (FileKind::Credentials, Some(prefix)) => Err(format!(
                "[{prefix} {suffix}]: credentials file sections take no prefix"
            )),
            (FileKind::Credentials, None) if !is_identifier(suffix) => {
                Err(format!("[{suffix}]: `{suffix}` is not a valid name"))
            }
            _ => Ok(self),
        }
    }
}

/// What the last line opened, for the lines that follow. `section` and
/// `property` index into the parsed sections and their properties.
enum Open {
    /// No section yet.
    Nothing,
    /// A section with no property yet.
    Section { section: usize },
    /// A property. When its value was empty, continuations are
    /// sub-properties and must be `key = value`.
    Property {
        section: usize,
        property: usize,
        sub_properties: bool,
    },
}

/// Splits `contents` into sections and their raw properties, in file order.
/// A section header repeated later in the file adds to the first.
fn parse<'a>(contents: &'a str, path: &Path) -> Result<Vec<RawSection<'a>>, ParseError> {
    let mut sections: Vec<RawSection<'a>> = Vec::new();
    let mut open = Open::Nothing;
    for (number, line) in contents.lines().enumerate() {
        let error = |message: String| ParseError {
            path: path.to_owned(),
            line: number + 1,
            message,
        };
        if line.trim_matches(WHITESPACE).is_empty() || line.starts_with(COMMENT) {
            continue;
        }
        if line.starts_with('[') {
            let header = strip_comment(line, CommentRule::Anywhere)
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .ok_or_else(|| error("Profile definition must end with ']'".to_owned()))?;
            let section = match sections.iter().position(|(h, _)| *h == header) {
                Some(section) => section,
                None => {
                    sections.push((header, Vec::new()));
                    sections.len() - 1
                }
            };
            open = Open::Section { section };
            continue;
        }
        let section = match open {
            Open::Nothing => return Err(error("Expected a profile definition".to_owned())),
            Open::Section { section } | Open::Property { section, .. } => section,
        };
        let properties = &mut sections[section].1;
        if line.starts_with(WHITESPACE) {
            let Open::Property {
                property: index,
                sub_properties,
                ..
            } = open
            else {
                return Err(error(
                    "Expected a property definition, found continuation".to_owned(),
                ));
            };
            let line = line.trim_matches(WHITESPACE);
            if sub_properties && !line.is_empty() {
                property(line).map_err(|e| error(e.message(LineKind::SubProperty)))?;
            }
            let value = &mut properties[index].1;
            value.push('\n');
            value.push_str(line);
            continue;
        }
        let (key, value) = property(line).map_err(|e| error(e.message(LineKind::Property)))?;
        let sub_properties = value.is_empty();
        let index = match properties.iter().position(|(k, _)| *k == key) {
            Some(index) => {
                properties[index].1 = value.to_owned();
                index
            }
            None => {
                properties.push((key, value.to_owned()));
                properties.len() - 1
            }
        };
        open = Open::Property {
            section,
            property: index,
            sub_properties,
        };
    }
    Ok(sections)
}

/// Which kind of `key = value` line failed, for the error message.
#[derive(Clone, Copy)]
enum LineKind {
    /// A property of a section.
    Property,
    /// An indented property under a property with an empty value.
    SubProperty,
}

/// Why a `key = value` line did not parse.
enum PropertyError {
    NoEquals,
    NoName,
}

impl PropertyError {
    /// The upstream message for this error on a line of `kind`.
    fn message(self, kind: LineKind) -> String {
        let (lower, capitalized) = match kind {
            LineKind::Property => ("property", "Property"),
            LineKind::SubProperty => ("sub-property", "Sub-property"),
        };
        match self {
            Self::NoEquals => format!("Expected an '=' sign defining a {lower}"),
            Self::NoName => format!("{capitalized} did not have a name"),
        }
    }
}

/// Parses `key = value`, dropping a trailing comment. The key is lowercased.
fn property(line: &str) -> Result<(String, &str), PropertyError> {
    let line = strip_comment(line, CommentRule::AfterWhitespace);
    let (key, value) = line.split_once('=').ok_or(PropertyError::NoEquals)?;
    let key = key.trim_matches(WHITESPACE);
    if key.is_empty() {
        return Err(PropertyError::NoName);
    }
    Ok((key.to_ascii_lowercase(), value.trim_matches(WHITESPACE)))
}

/// Where a comment may start on a line.
#[derive(Clone, Copy)]
enum CommentRule {
    /// Anywhere, as on a section header.
    Anywhere,
    /// Only after whitespace, so a value may hold `#` or `;`.
    AfterWhitespace,
}

/// Trims `line` and cuts it at the first comment.
fn strip_comment(line: &str, rule: CommentRule) -> &str {
    let line = line.trim_matches(WHITESPACE);
    let mut after_whitespace = false;
    let end = line
        .char_indices()
        .find(|&(_, c)| {
            let starts =
                COMMENT.contains(&c) && (after_whitespace || matches!(rule, CommentRule::Anywhere));
            after_whitespace = c.is_whitespace();
            starts
        })
        .map_or(line.len(), |(i, _)| i);
    line[..end].trim_matches(WHITESPACE)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    /// Reads a file of upstream cases from `profile-test-data/`, copied
    /// from `aws-runtime` 1.10.2 (Apache-2.0).
    fn cases(file: &str) -> Vec<Value> {
        let path = format!(
            "{}/src/aws/profile-test-data/{file}",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut json: Value = serde_json::from_str(&text).unwrap();
        let Value::Array(tests) = json["tests"].take() else {
            panic!("{path}: no tests array");
        };
        tests
    }

    fn as_map(value: &Value) -> HashMap<String, Properties> {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }

    /// Every case of the upstream profile parser suite: the parsed profiles
    /// and SSO sessions, or the error, match what the SDKs produce. Breaks
    /// if any parse, normalize or merge rule differs from upstream.
    #[test]
    fn upstream_parser_cases() {
        let cases = cases("profile-parser-tests.json");
        assert_eq!(cases.len(), 65);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = &case["input"];
            let mut sections = Sections::default();
            let result = [
                ("configFile", FileKind::Config),
                ("credentialsFile", FileKind::Credentials),
            ]
            .into_iter()
            .filter_map(|(field, kind)| Some((input[field].as_str()?, kind)))
            .try_for_each(|(contents, kind)| {
                merge(&mut sections, contents, kind, Path::new("test"))
            });
            let output = &case["output"];
            if let Some(expected) = output["errorContaining"].as_str() {
                let err = result.expect_err(name).to_string();
                assert!(err.contains(expected), "{name}: {err}");
                continue;
            }
            result.unwrap_or_else(|e| panic!("{name}: {e}"));
            let config = &output["config"];
            assert_eq!(sections.profiles, as_map(&config["profiles"]), "{name}");
            assert_eq!(
                sections.sso_sessions,
                as_map(&config["sso_sessions"]),
                "{name}"
            );
        }
    }

    /// Every case of the upstream file location suite, on both platforms.
    /// Breaks if home lookup, `~` expansion, an override variable or the
    /// profile variable is read differently from the SDKs.
    #[test]
    fn upstream_location_cases() {
        let cases = cases("file-location-tests.json");
        assert_eq!(cases.len(), 9);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let env = Env::Fixed(serde_json::from_value(case["environment"].clone()).unwrap());
            let platform = match case["platform"].as_str().unwrap() {
                "windows" => Platform::Windows,
                "linux" => Platform::Unix,
                other => panic!("{name}: unknown platform {other}"),
            };
            let found = Location::find(&env, platform);
            assert_eq!(
                found.config.path,
                PathBuf::from(case["configLocation"].as_str().unwrap()),
                "{name}"
            );
            assert_eq!(
                found.credentials.path,
                PathBuf::from(case["credentialsLocation"].as_str().unwrap()),
                "{name}"
            );
            if let Some(profile) = case["profile"].as_str() {
                assert_eq!(found.profile, profile, "{name}");
            }
        }
    }

    /// `[services name]` sub-properties land under the section and service,
    /// where the endpoint URL lookup reads them, and never in a profile.
    /// Breaks if a services section is dropped or read as a profile.
    #[test]
    fn services_section_sub_properties() {
        let mut sections = Sections::default();
        merge(
            &mut sections,
            "[services local]\necr =\n  endpoint_url = http://localhost:4566\n",
            FileKind::Config,
            Path::new("test"),
        )
        .unwrap();
        assert_eq!(
            sections.services["local"]["ecr"]["endpoint_url"],
            "http://localhost:4566"
        );
        assert!(sections.profiles.is_empty());
    }

    /// The credentials file wins over the config file for the same key,
    /// read through `load` from the located files. A missing file is empty.
    /// Breaks if the merge order flips.
    #[test]
    fn load_credentials_file_wins() {
        let env = Env::Fixed(HashMap::from([("HOME".to_owned(), "/home/u".to_owned())]));
        let fs = Fs::Fixed(HashMap::from([
            (
                PathBuf::from("/home/u/.aws/config"),
                b"[default]\nregion = us-west-2\naws_access_key_id = from-config\n".to_vec(),
            ),
            (
                PathBuf::from("/home/u/.aws/credentials"),
                b"[default]\naws_access_key_id = from-credentials\n".to_vec(),
            ),
        ]));
        let location = Location::find(&env, Platform::Unix);
        let sections = load(&fs, &location).unwrap();
        let default = &sections.profiles["default"];
        assert_eq!(default["aws_access_key_id"], "from-credentials");
        assert_eq!(default["region"], "us-west-2");

        let empty = load(&Fs::Fixed(HashMap::new()), &location).unwrap();
        assert!(empty.profiles.is_empty());
    }

    /// A file that is not UTF-8 is ignored rather than failing the load.
    /// Breaks if it becomes an error.
    #[test]
    fn load_ignores_non_utf8() {
        let env = Env::Fixed(HashMap::from([("HOME".to_owned(), "/h".to_owned())]));
        let fs = Fs::Fixed(HashMap::from([(
            PathBuf::from("/h/.aws/config"),
            vec![0xff, 0xfe],
        )]));
        let sections = load(&fs, &Location::find(&env, Platform::Unix)).unwrap();
        assert!(sections.profiles.is_empty());
    }
}
