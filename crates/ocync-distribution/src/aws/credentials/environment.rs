//! Credentials from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
//! `AWS_SESSION_TOKEN`, read as the SDK reads them.

use super::super::host::Env;
use super::{Credentials, Error};

/// The key pair in the environment. A key ID or secret that is unset or
/// blank is not configured; `SECRET_ACCESS_KEY` stands in for a missing
/// `AWS_SECRET_ACCESS_KEY`, and a blank session token is none.
pub(super) fn load(env: &Env) -> Result<Credentials, Error> {
    let set = |key: &str| env.get(key).filter(|value| !value.trim().is_empty());
    let not_set = || Error::NotLoaded("environment variable not set".to_owned());
    let access_key_id = set("AWS_ACCESS_KEY_ID").ok_or_else(not_set)?;
    let secret_access_key = set("AWS_SECRET_ACCESS_KEY")
        .or_else(|| set("SECRET_ACCESS_KEY"))
        .ok_or_else(not_set)?;
    let session_token = set("AWS_SESSION_TOKEN").map(|token| token.trim().to_owned());
    Ok(Credentials {
        access_key_id,
        secret_access_key,
        session_token,
        expiry: None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn load_from(vars: &[(&str, &str)]) -> Result<Credentials, Error> {
        let vars = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<HashMap<_, _>>();
        load(&Env::Fixed(vars))
    }

    /// The SDK's environment provider cases: a key pair with or without a
    /// session token, the legacy secret name, a blank token dropped, and a
    /// missing or blank key ID or secret not configured. Breaks if a blank
    /// value counts as set, which would stop the chain on an empty key.
    #[test]
    fn reads_the_key_pair_as_the_sdk_does() {
        let loaded = load_from(&[
            ("AWS_ACCESS_KEY_ID", "access"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_SESSION_TOKEN", " token "),
        ])
        .unwrap();
        assert_eq!(loaded.access_key_id(), "access");
        assert_eq!(loaded.secret_access_key(), "secret");
        assert_eq!(loaded.session_token(), Some("token"));

        let legacy = load_from(&[
            ("AWS_ACCESS_KEY_ID", "access"),
            ("AWS_SECRET_ACCESS_KEY", " "),
            ("SECRET_ACCESS_KEY", "secret"),
            ("AWS_SESSION_TOKEN", " "),
        ])
        .unwrap();
        assert_eq!(legacy.secret_access_key(), "secret");
        assert_eq!(legacy.session_token(), None);

        for vars in [
            &[][..],
            &[("AWS_SECRET_ACCESS_KEY", "secret")],
            &[("AWS_ACCESS_KEY_ID", " "), ("AWS_SECRET_ACCESS_KEY", "s")],
            &[("AWS_ACCESS_KEY_ID", "access")],
        ] {
            let err = load_from(vars).unwrap_err();
            assert!(matches!(err, Error::NotLoaded(_)), "{vars:?}: {err}");
        }
    }
}
