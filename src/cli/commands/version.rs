pub fn run() -> i32 {
    let version = env!("CARGO_PKG_VERSION");
    println!("ocync {version}");

    #[cfg(feature = "fips")]
    println!("FIPS 140-3: compiled=yes");

    #[cfg(not(feature = "fips"))]
    println!("FIPS 140-3: compiled=no");

    0
}
