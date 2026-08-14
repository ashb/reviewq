//! What `reviewq --version` says.
//!
//! `Cargo.toml` keeps the version of the last release, which is what a tag is
//! for and what a package needs. What it cannot say is where *this* build sits
//! relative to that tag — and a binary reporting `0.2.0` while running code from
//! four commits later is how an afternoon gets spent wondering why a feature
//! isn't there.
//!
//! So `git describe` answers instead, by way of `semvertag`, which turns its
//! output into something semver can order. That rewriting is the whole problem:
//! `0.2.0-10-ga5922f5` sorts *below* 0.2.0, because everything after the hyphen
//! is a pre-release and a pre-release precedes the version it qualifies. The
//! answer is `0.2.1-dev.10+ga5922f5` — above 0.2.0, below 0.2.1, in commit order
//! in between — and getting every corner of that right (a tag that is itself a
//! pre-release, a shallow clone that describes to a plausible lie) is more
//! fiddle than it looks.
//!
//! At a tag exactly, with nothing changed, the version is the tag: a release
//! build should read as the release. Where git cannot answer — a tarball, a
//! clone without tags, a shallow CI checkout — the manifest's version is the
//! fallback, which is exactly the answer reviewq gave before this existed.

fn main() {
    println!("cargo:rustc-env=REVIEWQ_VERSION={}", version());
    watch_git();
}

fn version() -> String {
    let manifest = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());

    // The manifest as a *hint*, which is only taken when it is a legal single
    // step past the tag. Holding the last released version, as it does here, it
    // never is, so the derivation falls back to bumping the patch — but on the
    // day the next release is known to be a minor, bumping the manifest early is
    // all it takes for the dev builds to say `0.3.0-dev.N` and mean it.
    let hint = semver::Version::parse(&manifest).ok();
    match semvertag_shell::describe_with_hint(hint.as_ref()) {
        Ok(version) => version.to_string(),
        // No git at all is the packaged build: the manifest's version is the
        // right answer and there is nothing to say about it.
        Err(semvertag_shell::ShellError::GitUnavailable) => manifest,
        // Everything else means git was there and the derivation still failed —
        // a shallow CI checkout, or a tag that is not a version sitting nearer
        // than the one that is. Falling back is still right, but silently
        // reverting to the last release is how a version quietly stops tracking,
        // so say which it was.
        Err(err) => {
            println!("cargo:warning=using the manifest version {manifest}: {err}");
            manifest
        }
    }
}

/// Ask cargo to re-run this when the answer could have changed: a commit, a
/// moved branch, a new tag.
///
/// Without this the version goes stale the first time a commit touches no source
/// file. *With* it done carelessly it is worse: cargo re-runs a build script
/// unconditionally when a watched path is missing, so naming `.git/packed-refs`
/// in a repository that has none — or any of these in a tarball that has no
/// `.git` at all — relinks the binary on every single build.
///
/// So: the real git directory, asked for rather than assumed, since `.git` is a
/// *file* in a worktree or a submodule; and only the paths actually there.
fn watch_git() {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output();
    let Some(git_dir) = out.ok().filter(|out| out.status.success()) else {
        return;
    };
    let git_dir = String::from_utf8_lossy(&git_dir.stdout).trim().to_string();

    // `refs` as a directory, not the current branch's file: a tag is what the
    // version is built from, and a new one lands under `refs/tags`.
    for path in ["HEAD", "refs", "packed-refs"] {
        let path = std::path::Path::new(&git_dir).join(path);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
