//! The profile source: the selected profile of the shared config and
//! credentials files, resolved as `aws-config` 1.13.0 resolves it into the
//! source of its first credentials and the roles assumed from them.

use std::collections::HashMap;

use super::super::endpoint::{InvalidRegion, Region};
use super::super::host::Clock;
use super::super::profile::{DEFAULT_PROFILE, ParseError, Properties, Sections};
use super::sso_token::Login;
use super::web_identity::{self, Origin};
use super::{Credentials, Error as ChainError, Source, process, sso, sts};

const ROLE_ARN: &str = "role_arn";
const EXTERNAL_ID: &str = "external_id";
const ROLE_SESSION_NAME: &str = "role_session_name";
const CREDENTIAL_SOURCE: &str = "credential_source";
const SOURCE_PROFILE: &str = "source_profile";
const WEB_IDENTITY_TOKEN_FILE: &str = "web_identity_token_file";
const ACCESS_KEY_ID: &str = "aws_access_key_id";
const SECRET_ACCESS_KEY: &str = "aws_secret_access_key";
const SESSION_TOKEN: &str = "aws_session_token";
const CREDENTIAL_PROCESS: &str = "credential_process";
const LOGIN_SESSION: &str = "login_session";
const SSO_ACCOUNT_ID: &str = "sso_account_id";
const SSO_REGION: &str = "sso_region";
const SSO_ROLE_NAME: &str = "sso_role_name";
const SSO_START_URL: &str = "sso_start_url";
const SSO_SESSION: &str = "sso_session";

/// The prefix of the session name the SDK makes for a role a profile
/// assumes.
const ROLE_SESSION_PREFIX: &str = "assume-role-from-profile";

/// A source of the chain a profile names with `credential_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Named {
    /// `Environment`: the access key variables.
    Environment,
    /// `Ec2InstanceMetadata`: the instance metadata service.
    Ec2InstanceMetadata,
    /// `EcsContainer`: the container endpoint.
    EcsContainer,
}

impl Named {
    /// The source `name` names, matched without regard to case as upstream.
    fn parse(name: &str) -> Option<Self> {
        [
            ("Environment", Self::Environment),
            ("Ec2InstanceMetadata", Self::Ec2InstanceMetadata),
            ("EcsContainer", Self::EcsContainer),
        ]
        .into_iter()
        .find_map(|(text, named)| text.eq_ignore_ascii_case(name).then_some(named))
    }

    /// The chain's source of that name.
    pub(super) fn source(self) -> Source {
        match self {
            Self::Environment => Source::Environment,
            Self::Ec2InstanceMetadata => Source::Imds,
            Self::EcsContainer => Source::Container,
        }
    }
}

/// Where a profile's first credentials come from.
#[derive(Debug)]
pub(super) enum Base<'a> {
    /// A source of the chain, named by `credential_source`.
    Named(Named),
    /// The keys the profile holds.
    Keys(Credentials),
    /// A role assumed with a web identity token file.
    WebIdentity(web_identity::Role<'a>),
    /// The command `credential_process` names.
    Process(process::Command<'a>),
    /// An IAM Identity Center role.
    Sso(sso::Role<'a>),
}

/// A role assumed with the credentials before it.
#[derive(Debug)]
pub(super) struct AssumedRole<'a> {
    arn: &'a str,
    external_id: Option<&'a str>,
    session_name: Option<&'a str>,
}

impl AssumedRole<'_> {
    /// The role's credentials, from STS in the region `sts` calls, signed
    /// with `caller`. With no session name set, the SDK's is made from the
    /// time on `clock`.
    pub(super) async fn assume(
        &self,
        sts: &sts::Client,
        caller: &Credentials,
        clock: Clock,
    ) -> Result<Credentials, Error> {
        let session_name = sts::session_name(self.session_name, ROLE_SESSION_PREFIX, clock);
        let call = sts::AssumeRole {
            role_arn: self.arn,
            session_name: &session_name,
            external_id: self.external_id,
        };
        sts.assume_role(caller, &call)
            .await
            .map_err(|source| Error::AssumeRole {
                arn: self.arn.to_owned(),
                source,
            })
    }
}

/// A resolved profile: its base, and the roles assumed from the base's
/// credentials, in order.
#[derive(Debug)]
pub(super) struct Plan<'a> {
    pub(super) base: Base<'a>,
    pub(super) roles: Vec<AssumedRole<'a>>,
}

/// What a profile's base settings name.
enum Found<'a> {
    /// A base of credentials.
    Base(Base<'a>),
    /// An SSO login token alone, which no credentials come from.
    Token,
}

/// What upstream prints when a profile's base credentials do not load.
const BASE_FAILED: &str = "an error occurred while loading credentials";

/// Why a profile gave no credentials. The texts are upstream's.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Error {
    /// The shared files do not parse.
    #[error("could not parse profile file")]
    Parse(#[source] ParseError),
    /// No profile, or no `default` profile when it is selected.
    #[error("No profiles were defined")]
    NoProfiles,
    /// The selected profile holds nothing to load credentials from.
    #[error("profile `{0}` did not contain credential information")]
    NoCredentials(String),
    /// A profile the chain names does not exist.
    #[error(
        "profile `{profile}` was not defined: could not find source profile {profile} referenced from {}",
        .from.as_deref().unwrap_or("the root profile")
    )]
    MissingProfile {
        profile: String,
        /// The profile that named it. None for the selected profile.
        from: Option<String>,
    },
    /// `source_profile` leads back to a profile already visited.
    #[error(
        "profile formed an infinite loop. first we loaded {visited:?}, then attempted to reload {next}"
    )]
    Loop { visited: Vec<String>, next: String },
    /// A profile's credential settings do not fit together.
    #[error("invalid credential source in `{profile}`: {problem}")]
    Invalid { profile: String, problem: Problem },
    /// `credential_source` names no source.
    #[error("profile referenced `{0}` provider but that provider is not supported")]
    UnknownSource(String),
    /// `login_session`, which ocync does not support.
    #[error("profile `{0}` uses login_session, which is not supported")]
    LoginSession(String),
    /// A profile's SSO settings do not fit together.
    #[error("profile `{profile}` has invalid SSO config: {problem}")]
    InvalidSso {
        profile: String,
        problem: SsoProblem,
    },
    /// `sso_session` names no `[sso-session]` section.
    #[error("sso-session named `{session}` (referenced by profile `{profile}`) was not found")]
    MissingSsoSession { profile: String, session: String },
    /// An SSO profile lacks a setting it needs.
    #[error("profile `{profile}` was not defined: `{field}` was missing")]
    MissingField {
        profile: String,
        field: &'static str,
    },
    /// The SSO profile names a login token alone, for the services that
    /// take one.
    #[error(
        "selected profile will resolve an access token instead of credentials since it doesn't have `sso_account_id` and `sso_role_name` set. Specify both `sso_account_id` and `sso_role_name` to let this profile resolve credentials."
    )]
    TokenProfile,
    /// The base credentials did not load from a source of the chain.
    #[error("{BASE_FAILED}")]
    Base(#[source] Box<ChainError>),
    /// The base credentials did not load from `credential_process`.
    #[error("{BASE_FAILED}")]
    Process(#[source] process::Error),
    /// The base credentials did not load from IAM Identity Center.
    #[error("{BASE_FAILED}")]
    Sso(#[source] sso::Error),
    /// A role was not assumed.
    #[error("failed to assume role {arn}")]
    AssumeRole {
        arn: String,
        #[source]
        source: sts::Error,
    },
}

/// What is wrong with a profile's credential settings.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum Problem {
    /// A role names both where its credentials come from.
    #[error(
        "profile contained both source_profile and credential_source. Only one or the other can be defined"
    )]
    BothSources,
    /// A role names neither.
    #[error(
        "profile must contain `source_profile` or `credential_source` but neither were defined"
    )]
    NoSource,
    /// A token file with no role to assume with it.
    #[error("`web_identity_token_file` was specified but `role_arn` was missing")]
    TokenFileWithoutRole,
    /// A secret key with no key ID.
    #[error("profile missing aws_access_key_id")]
    NoAccessKeyId,
    /// A key ID with no secret key.
    #[error("profile missing aws_secret_access_key")]
    NoSecretAccessKey,
    /// A source profile failed. Its error is part of the text, as upstream
    /// prints it.
    #[error("could not load source profile: {0}")]
    SourceProfile(Box<Error>),
}

/// What is wrong with a profile's SSO settings.
#[derive(Debug, thiserror::Error)]
pub(in crate::aws) enum SsoProblem {
    /// A setting an `[sso-session]` holds, set on a profile that names
    /// one.
    #[error("`{0}` can only be specified in the [sso-session] config when a session name is given")]
    OutsideSession(&'static str),
    /// One of the account and role is set without the other.
    #[error("if `{set}` is set, then `{missing}` must also be set")]
    Unpaired {
        set: &'static str,
        missing: &'static str,
    },
    /// `sso_region` is not a region.
    #[error("invalid `sso_region`: {0}")]
    Region(InvalidRegion),
}

/// Resolve the profile `selected` in `sections` as upstream does: follow
/// `source_profile`, each `role_arn` adding a role, until a profile gives
/// the base. A source profile with keys is the base whatever else it holds.
pub(super) fn resolve<'a>(sections: &'a Sections, selected: &'a str) -> Result<Plan<'a>, Error> {
    let (profiles, sessions) = (&sections.profiles, &sections.sso_sessions);
    if profiles.is_empty() || (selected == DEFAULT_PROFILE && !profiles.contains_key(selected)) {
        return Err(Error::NoProfiles);
    }
    let mut name = selected;
    let mut visited: Vec<&str> = Vec::new();
    let mut roles = Vec::new();
    let found = loop {
        let profile = profiles.get(name).ok_or_else(|| Error::MissingProfile {
            profile: name.to_owned(),
            from: visited.last().map(ToString::to_string),
        })?;
        if visited.contains(&name) {
            return Err(Error::Loop {
                visited: visited.iter().map(ToString::to_string).collect(),
                next: name.to_owned(),
            });
        }
        visited.push(name);
        let root = visited.len() == 1;
        if !root && let Ok(keys) = keys(name, profile) {
            break Found::Base(Base::Keys(keys));
        }
        let Some(role) = assumed_role(profile) else {
            break base(name, profile, sessions).map_err(|e| {
                if root {
                    e
                } else {
                    invalid(name, Problem::SourceProfile(Box::new(e)))
                }
            })?;
        };
        roles.push(role);
        match next(name, profile)? {
            Next::Itself => break base(name, profile, sessions)?,
            Next::Profile(source) => name = source,
        }
    };
    match found {
        Found::Base(base) => {
            roles.reverse();
            Ok(Plan { base, roles })
        }
        Found::Token => Err(Error::TokenProfile),
    }
}

/// The property `key` of `profile`.
fn get<'a>(profile: &'a Properties, key: &str) -> Option<&'a str> {
    profile.get(key).map(String::as_str)
}

fn invalid(profile: &str, problem: Problem) -> Error {
    Error::Invalid {
        profile: profile.to_owned(),
        problem,
    }
}

/// The role `profile` assumes, unless it is a web identity profile, whose
/// `role_arn` is the base's.
fn assumed_role(profile: &Properties) -> Option<AssumedRole<'_>> {
    if profile.contains_key(WEB_IDENTITY_TOKEN_FILE) {
        return None;
    }
    Some(AssumedRole {
        arn: get(profile, ROLE_ARN)?,
        external_id: get(profile, EXTERNAL_ID),
        session_name: get(profile, ROLE_SESSION_NAME),
    })
}

/// Where a profile that assumes a role gets the credentials to assume it.
enum Next<'a> {
    /// From its own base.
    Itself,
    /// From this profile.
    Profile(&'a str),
}

fn next<'a>(name: &str, profile: &'a Properties) -> Result<Next<'a>, Error> {
    match (
        get(profile, SOURCE_PROFILE),
        profile.contains_key(CREDENTIAL_SOURCE),
    ) {
        (Some(_), true) => Err(invalid(name, Problem::BothSources)),
        (None, false) => Err(invalid(name, Problem::NoSource)),
        (Some(source), false) if source == name => Ok(Next::Itself),
        (Some(source), false) => Ok(Next::Profile(source)),
        (None, true) => Ok(Next::Itself),
    }
}

/// The base `profile` names, in upstream's order: `credential_source`,
/// web identity, SSO with the `[sso-session]` sections `sessions`,
/// `login_session`, `credential_process`, keys.
fn base<'a>(
    name: &str,
    profile: &'a Properties,
    sessions: &'a HashMap<String, Properties>,
) -> Result<Found<'a>, Error> {
    if let Some(source) = get(profile, CREDENTIAL_SOURCE) {
        return Named::parse(source)
            .map(|named| Found::Base(Base::Named(named)))
            .ok_or_else(|| Error::UnknownSource(source.to_owned()));
    }
    match (
        get(profile, ROLE_ARN),
        get(profile, WEB_IDENTITY_TOKEN_FILE),
    ) {
        (Some(arn), Some(token_file)) => {
            return Ok(Found::Base(Base::WebIdentity(web_identity::Role {
                arn,
                token_file,
                session_name: get(profile, ROLE_SESSION_NAME),
                origin: Origin::Profile,
            })));
        }
        (None, Some(_)) => return Err(invalid(name, Problem::TokenFileWithoutRole)),
        (_, None) => {}
    }
    if let Some(found) = sso(name, profile, sessions)? {
        return Ok(found);
    }
    if profile.contains_key(LOGIN_SESSION) {
        return Err(Error::LoginSession(name.to_owned()));
    }
    if let Some(command) = get(profile, CREDENTIAL_PROCESS) {
        return Ok(Found::Base(Base::Process(process::Command::new(command))));
    }
    keys(name, profile).map(|keys| Found::Base(Base::Keys(keys)))
}

/// The SSO base `profile` names, as upstream reads it, or `None` when it
/// has no SSO setting. A profile that names an `[sso-session]` takes its
/// region and start URL from it, and must not set them itself.
fn sso<'a>(
    name: &str,
    profile: &'a Properties,
    sessions: &'a HashMap<String, Properties>,
) -> Result<Option<Found<'a>>, Error> {
    let account_id = get(profile, SSO_ACCOUNT_ID);
    let role_name = get(profile, SSO_ROLE_NAME);
    let session = get(profile, SSO_SESSION);
    let mut region = get(profile, SSO_REGION);
    let mut start_url = get(profile, SSO_START_URL);
    if [account_id, role_name, session, region, start_url]
        .iter()
        .all(Option::is_none)
    {
        return Ok(None);
    }
    let invalid_sso = |problem| Error::InvalidSso {
        profile: name.to_owned(),
        problem,
    };
    if let Some(session) = session {
        for (key, value) in [(SSO_START_URL, start_url), (SSO_REGION, region)] {
            if value.is_some() {
                return Err(invalid_sso(SsoProblem::OutsideSession(key)));
            }
        }
        let section = sessions
            .get(session)
            .ok_or_else(|| Error::MissingSsoSession {
                profile: name.to_owned(),
                session: session.to_owned(),
            })?;
        region = get(section, SSO_REGION);
        start_url = get(section, SSO_START_URL);
    }
    let role = match (account_id, role_name) {
        (Some(account_id), Some(role_name)) => Some((account_id, role_name)),
        (None, None) => None,
        (Some(_), None) => {
            return Err(invalid_sso(SsoProblem::Unpaired {
                set: SSO_ACCOUNT_ID,
                missing: SSO_ROLE_NAME,
            }));
        }
        (None, Some(_)) => {
            return Err(invalid_sso(SsoProblem::Unpaired {
                set: SSO_ROLE_NAME,
                missing: SSO_ACCOUNT_ID,
            }));
        }
    };
    let missing = |field| Error::MissingField {
        profile: name.to_owned(),
        field,
    };
    let region = region.ok_or_else(|| missing(SSO_REGION))?;
    let start_url = start_url.ok_or_else(|| missing(SSO_START_URL))?;
    let Some((account_id, role_name)) = role else {
        return Ok(Some(Found::Token));
    };
    let region = Region::parse(region).map_err(|e| invalid_sso(SsoProblem::Region(e)))?;
    let login = match session {
        Some(name) => Login::Session { name, start_url },
        None => Login::Legacy { start_url },
    };
    Ok(Some(Found::Base(Base::Sso(sso::Role {
        login,
        region,
        account_id,
        role_name,
    }))))
}

/// The keys `profile` holds.
fn keys(name: &str, profile: &Properties) -> Result<Credentials, Error> {
    let session_token = get(profile, SESSION_TOKEN);
    match (
        get(profile, ACCESS_KEY_ID),
        get(profile, SECRET_ACCESS_KEY),
        session_token,
    ) {
        (None, None, None) => Err(Error::NoCredentials(name.to_owned())),
        (None, _, _) => Err(invalid(name, Problem::NoAccessKeyId)),
        (Some(_), None, _) => Err(invalid(name, Problem::NoSecretAccessKey)),
        (Some(id), Some(secret), _) => Ok(Credentials {
            access_key_id: id.to_owned(),
            secret_access_key: secret.to_owned(),
            session_token: session_token.map(str::to_owned),
            expiry: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use serde::Deserialize;

    use super::super::fixtures::DATA;
    use super::*;

    /// The rows not run, by their `docs`. Upstream resolves a profile that
    /// names only a login token and refuses it when it builds the
    /// provider; here it is refused at once, as
    /// [`reads_sso_bases_as_upstream_does`] checks.
    const EXCLUDED: &[&str] = &["SSO token profile selected"];

    #[derive(Deserialize)]
    struct Input {
        profiles: HashMap<String, Properties>,
        #[serde(default)]
        sso_sessions: HashMap<String, Properties>,
        selected_profile: String,
    }

    /// One step of a resolved profile, as upstream's cases write it. The
    /// account ID is left out: the owned chain does not carry it.
    #[derive(Debug, Deserialize, PartialEq, Eq)]
    enum Step {
        AccessKey {
            access_key_id: String,
            secret_access_key: String,
            session_token: Option<String>,
        },
        NamedSource(String),
        WebIdentityToken {
            role_arn: String,
            web_identity_token_file: String,
            role_session_name: Option<String>,
        },
        CredentialProcess {
            command: String,
        },
        Sso {
            sso_session: Option<String>,
            sso_region: String,
            sso_start_url: String,
            sso_account_id: String,
            sso_role_name: String,
        },
        AssumeRole {
            role_arn: String,
            external_id: Option<String>,
            role_session_name: Option<String>,
        },
    }

    #[derive(Debug, Deserialize)]
    enum Output {
        ProfileChain(Vec<Step>),
        Error(String),
    }

    fn steps(plan: &Plan<'_>) -> Vec<Step> {
        let Plan { base, roles } = plan;
        let base = match base {
            Base::Named(named) => Step::NamedSource(format!("{named:?}")),
            Base::Keys(keys) => Step::AccessKey {
                access_key_id: keys.access_key_id.clone(),
                secret_access_key: keys.secret_access_key.clone(),
                session_token: keys.session_token.clone(),
            },
            Base::WebIdentity(role) => Step::WebIdentityToken {
                role_arn: role.arn.to_owned(),
                web_identity_token_file: role.token_file.to_owned(),
                role_session_name: role.session_name.map(str::to_owned),
            },
            Base::Process(command) => Step::CredentialProcess {
                command: command.unredacted().to_owned(),
            },
            Base::Sso(role) => {
                let (session, start_url) = match role.login {
                    Login::Session { name, start_url } => (Some(name.to_owned()), start_url),
                    Login::Legacy { start_url } => (None, start_url),
                };
                Step::Sso {
                    sso_session: session,
                    sso_region: role.region.as_str().to_owned(),
                    sso_start_url: start_url.to_owned(),
                    sso_account_id: role.account_id.to_owned(),
                    sso_role_name: role.role_name.to_owned(),
                }
            }
        };
        let roles = roles.iter().map(|role| Step::AssumeRole {
            role_arn: role.arn.to_owned(),
            external_id: role.external_id.map(str::to_owned),
            role_session_name: role.session_name.map(str::to_owned),
        });
        std::iter::once(base).chain(roles).collect()
    }

    /// Every upstream resolution case not excluded resolves to the same
    /// base and roles, or fails with upstream's text. Breaks if a source
    /// profile, a loop, a role or a base is read differently.
    #[test]
    fn resolves_as_upstream_does() {
        let text = std::fs::read_to_string(Path::new(DATA).join("assume-role-tests.json")).unwrap();
        let cases: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        assert_eq!(cases.len(), 28);
        let mut run = 0;
        for case in cases {
            let docs = case["docs"].as_str().unwrap();
            if EXCLUDED.contains(&docs) {
                continue;
            }
            let input: Input = serde_json::from_value(case["input"].clone()).unwrap();
            let sections = Sections {
                profiles: input.profiles,
                sso_sessions: input.sso_sessions,
                ..Sections::default()
            };
            let resolved = resolve(&sections, &input.selected_profile);
            match (
                serde_json::from_value(case["output"].clone()).unwrap(),
                resolved,
            ) {
                (Output::ProfileChain(expected), Ok(plan)) => {
                    assert_eq!(steps(&plan), expected, "{docs}");
                }
                (Output::Error(text), Err(e)) => {
                    let error = e.to_string();
                    assert!(error.contains(&text), "{docs}: {error}");
                }
                (expected, got) => panic!("{docs}: expected {expected:?}, got {got:?}"),
            }
            run += 1;
        }
        assert_eq!(run + EXCLUDED.len(), 28);
    }

    /// An SSO source profile is the base of the role above it, its token
    /// named for its session. One that names only a login token is not
    /// configured at any depth, as upstream finds that after resolving. A
    /// missing `sso_region` is not configured at the root and fails a
    /// source profile. An `sso_region` that is not a region is refused as
    /// the profile is read. Breaks if the role above SSO is dropped, a
    /// token profile is read as credentials, the depth rule differs, or a
    /// bad region reaches a load.
    #[test]
    fn reads_sso_bases_as_upstream_does() {
        let mut sections = role_from(&[
            (SSO_SESSION, "dev"),
            (SSO_ACCOUNT_ID, "1"),
            (SSO_ROLE_NAME, "R"),
        ]);
        sections.sso_sessions.insert(
            "dev".to_owned(),
            HashMap::from([
                (SSO_REGION.to_owned(), "us-east-1".to_owned()),
                (
                    SSO_START_URL.to_owned(),
                    "https://d.awsapps.com/start".to_owned(),
                ),
            ]),
        );
        let plan = resolve(&sections, "A").unwrap();
        assert_eq!(
            steps(&plan),
            [
                Step::Sso {
                    sso_session: Some("dev".to_owned()),
                    sso_region: "us-east-1".to_owned(),
                    sso_start_url: "https://d.awsapps.com/start".to_owned(),
                    sso_account_id: "1".to_owned(),
                    sso_role_name: "R".to_owned(),
                },
                Step::AssumeRole {
                    role_arn: "arn:aws:iam::1:role/A".to_owned(),
                    external_id: None,
                    role_session_name: None,
                },
            ]
        );
        let source = sections.profiles.get_mut("B").unwrap();
        source.retain(|key, _| key == SSO_SESSION);
        for selected in ["A", "B"] {
            let error = resolve(&sections, selected).unwrap_err();
            assert!(matches!(error, Error::TokenProfile), "{selected}: {error}");
        }

        let legacy = role_from(&[
            (SSO_START_URL, "https://d.awsapps.com/start"),
            (SSO_ACCOUNT_ID, "1"),
            (SSO_ROLE_NAME, "R"),
        ]);
        let missing = "profile `B` was not defined: `sso_region` was missing";
        let error = resolve(&legacy, "B").unwrap_err();
        assert!(matches!(error, Error::MissingField { .. }), "{error}");
        assert_eq!(error.to_string(), missing);
        let error = resolve(&legacy, "A").unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("invalid credential source in `B`: could not load source profile: {missing}")
        );

        let bad_region = role_from(&[
            (SSO_START_URL, "https://d.awsapps.com/start"),
            (SSO_REGION, "us east 1"),
            (SSO_ACCOUNT_ID, "1"),
            (SSO_ROLE_NAME, "R"),
        ]);
        let error = resolve(&bad_region, "B").unwrap_err();
        assert!(
            matches!(
                error,
                Error::InvalidSso {
                    problem: SsoProblem::Region(_),
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("'us east 1'"), "{error}");
    }

    /// A `credential_process` source profile is the base of the role, and
    /// `login_session` is refused. Breaks if either kind is read as keys.
    #[test]
    fn reads_credential_process_and_login_session_bases() {
        let sections = role_from(&[(CREDENTIAL_PROCESS, "/bin/creds --profile x")]);
        let plan = resolve(&sections, "A").unwrap();
        assert_eq!(
            steps(&plan),
            [
                Step::CredentialProcess {
                    command: "/bin/creds --profile x".to_owned()
                },
                Step::AssumeRole {
                    role_arn: "arn:aws:iam::1:role/A".to_owned(),
                    external_id: None,
                    role_session_name: None,
                },
            ]
        );
        let sections = role_from(&[(LOGIN_SESSION, "arn:aws:signin::1:session")]);
        let error = resolve(&sections, "B").unwrap_err();
        assert!(matches!(error, Error::LoginSession(_)), "{error}");
    }

    /// `credential_source` is matched without regard to case, and a name
    /// that is no source is refused, on a source profile with the prefix
    /// that names it as one. Breaks if the match is exact or an unknown
    /// name is read as a source.
    #[test]
    fn reads_credential_source_as_upstream_does() {
        let profile = |source: &str| Sections {
            profiles: HashMap::from([(
                "A".to_owned(),
                HashMap::from([(CREDENTIAL_SOURCE.to_owned(), source.to_owned())]),
            )]),
            ..Sections::default()
        };
        let sections = profile("ec2instancemetadata");
        let plan = resolve(&sections, "A").unwrap();
        assert!(
            matches!(
                plan,
                Plan {
                    base: Base::Named(Named::Ec2InstanceMetadata),
                    ..
                }
            ),
            "{plan:?}"
        );
        let unknown = "profile referenced `Bogus` provider but that provider is not supported";
        let sections = profile("Bogus");
        let error = resolve(&sections, "A").unwrap_err();
        assert_eq!(error.to_string(), unknown);
        let sections = role_from(&[(CREDENTIAL_SOURCE, "Bogus")]);
        let error = resolve(&sections, "A").unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("invalid credential source in `B`: could not load source profile: {unknown}")
        );
    }

    /// Profile `A` assuming a role from source profile `B`, which holds
    /// only `base`.
    fn role_from(base: &[(&str, &str)]) -> Sections {
        Sections {
            profiles: HashMap::from([
                (
                    "A".to_owned(),
                    HashMap::from([
                        (ROLE_ARN.to_owned(), "arn:aws:iam::1:role/A".to_owned()),
                        (SOURCE_PROFILE.to_owned(), "B".to_owned()),
                    ]),
                ),
                (
                    "B".to_owned(),
                    base.iter()
                        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                        .collect(),
                ),
            ]),
            ..Sections::default()
        }
    }
}
