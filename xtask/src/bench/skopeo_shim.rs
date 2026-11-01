//! The skopeo dregsy runs: the real one, with `--preserve-digests` on every
//! `copy`.
//!
//! Without it skopeo compresses an uncompressed layer
//! (`application/vnd.docker.image.rootfs.diff.tar`) when it pushes to a
//! registry, so the target manifest has another digest and the copy is not the
//! source content. With it skopeo fails a copy that cannot keep the digest.
//! dregsy builds skopeo's command line from fixed flags and its config can set
//! only the binary, so the binary it is given is a script that runs this
//! module's subcommand, which runs skopeo.

use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The skopeo flag that keeps the source digest or fails the copy.
const PRESERVE_DIGESTS: &str = "--preserve-digests";

/// `args` for skopeo, with [`PRESERVE_DIGESTS`] right after a `copy`
/// subcommand. The subcommand is the first argument that is not a flag: dregsy
/// gives skopeo only flags without a value before it. Any other subcommand
/// (`list-tags`, `inspect`, `--version`) is returned unchanged.
fn preserve_digests(mut args: Vec<String>) -> Vec<String> {
    let copy = args
        .iter()
        .position(|arg| !arg.starts_with('-'))
        .filter(|&at| args[at] == "copy");
    if let Some(at) = copy {
        args.insert(at + 1, PRESERVE_DIGESTS.to_owned());
    }
    args
}

/// Run skopeo from `PATH` with `args`, as [`preserve_digests`] rewrites them,
/// in place of this process, so skopeo's exit status and signals are the
/// caller's.
pub(crate) fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let error = Command::new("skopeo").args(preserve_digests(args)).exec();
    Err(format!("failed to run skopeo: {error}").into())
}

/// The text of the script that runs this binary's shim subcommand.
fn script(xtask: &Path) -> String {
    let quoted = xtask.display().to_string().replace('\'', r"'\''");
    format!("#!/bin/sh\nexec '{quoted}' bench-skopeo \"$@\"\n")
}

/// Write the script dregsy is given as its skopeo into `dir`, running this
/// binary, and return its path.
pub(crate) fn write_script(dir: &Path) -> Result<PathBuf, String> {
    let xtask = std::env::current_exe().map_err(|e| format!("find this binary: {e}"))?;
    let path = dir.join("skopeo");
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o755)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, script(&xtask).as_bytes()));
    written.map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(args: &[&str]) -> Vec<String> {
        preserve_digests(args.iter().map(|a| (*a).to_owned()).collect())
    }

    #[test]
    fn only_a_copy_gains_the_flag_and_only_once_right_after_it() {
        // Catches: the flag on `list-tags` or `inspect`, which reject it; none
        // on `copy`, so skopeo compresses uncompressed layers; and a second
        // one when a later argument is also `copy`.
        let cases: [(&[&str], &[&str]); 5] = [
            (
                &[
                    "--insecure-policy",
                    "copy",
                    "--all",
                    "docker://a",
                    "docker://b",
                ],
                &[
                    "--insecure-policy",
                    "copy",
                    "--preserve-digests",
                    "--all",
                    "docker://a",
                    "docker://b",
                ],
            ),
            (&["list-tags", "docker://a"], &["list-tags", "docker://a"]),
            (
                &["--insecure-policy", "inspect", "--raw", "copy"],
                &["--insecure-policy", "inspect", "--raw", "copy"],
            ),
            (&["--version"], &["--version"]),
            (
                &["copy", "docker://a", "copy"],
                &["copy", "--preserve-digests", "docker://a", "copy"],
            ),
        ];
        for (args, want) in cases {
            assert_eq!(rewritten(args), want, "{args:?}");
        }
    }

    #[test]
    fn the_script_runs_this_binary_with_the_shim_subcommand_and_quotes_its_path() {
        // Catches: a path with a quote or a space ending the command early, and
        // arguments dropped on the way to skopeo.
        assert_eq!(
            script(Path::new("/opt/it's here/xtask")),
            "#!/bin/sh\nexec '/opt/it'\\''s here/xtask' bench-skopeo \"$@\"\n"
        );
    }
}
