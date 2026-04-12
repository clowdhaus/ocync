//! The `version` subcommand — shows version and build information.

pub(crate) fn run() -> i32 {
    let version = env!("CARGO_PKG_VERSION");
    println!("ocync {version}");

    // FIPS status is always compiled in — no feature flag needed.
    // aws-lc-rs is linked via ocync-distribution; if it was built in FIPS mode,
    // the aws-lc-sys build script sets the appropriate cfg. At runtime, we can
    // check whether the FIPS module was initialized.
    println!("FIPS 140-3: compiled=no");

    0
}
