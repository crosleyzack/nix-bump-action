//! Bump nixpkgs and home-manager release pins in `*.nix` files to the newest
//! upstream NixOS release.
//!
//! `flake.lock` is not touched either. Re-locking should be done with the
//! existing `DeterminateSystems/update-flake-lock`'s job.
//!
//! The target release comes from home-manager, not nixpkgs. home-manager only
//! cuts `release-XX.YY` once nixpkgs has cut `nixos-XX.YY`, so its newest
//! release can never run ahead of nixpkgs.

use anyhow::{bail, Result};
use clap::Parser;
use std::collections::HashSet;
use std::env;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

/// A NixOS release, always `YY.MM` with `MM` of `05` or `11` in practice.
#[derive(Debug, Hash, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    year: u8,
    month: u8,
}

impl Version {
    /// Parses exactly `dd.dd`. The strict length is what rejects refs like
    /// `nixos-25.05-small` when scanning branch names.
    fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 5 || b[2] != b'.' {
            return None;
        }
        // validate every item other than '.' is a number
        if !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 2 || c.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            year: s[0..2].parse().ok()?,
            month: s[3..5].parse().ok()?,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}.{:02}", self.year, self.month)
    }
}

struct Pattern {
    /// Matched case-insensitively, and covers everything up to the version so
    /// that only the five version bytes are ever replaced. Anything after the
    /// version, such as the `-small` in `nixos-25.05-small`, is left alone.
    prefix: &'static str,
}

const PATTERNS: &[Pattern] = &[
    Pattern {
        prefix: "github:nixos/nixpkgs/nixos-",
    },
    Pattern {
        prefix: "github:nix-community/home-manager/release-",
    },
];

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct Pin {
    start: usize,
    end: usize,
    version: Version,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct Bump {
    from: Pin,
    to: Version,
}

/// Every `prefix`-anchored version in `text`, in order of appearance.
fn all_pins(text: &str, prefix: &str) -> HashSet<Pin> {
    let mut out = HashSet::new();
    for (i, _) in text.match_indices(prefix) {
        let start = i + prefix.len();
        let end = start + 5;
        match text.get(start..end).and_then(Version::parse) {
            Some(version) => {
                out.insert(Pin {
                    start,
                    end,
                    version,
                });
            }
            // A prefix with no version after it, e.g. a `nixos-unstable` URL.
            None => log::info!("skipping non-version pin {:?}", text.get(start..end)),
        }
    }
    out
}

/// every pin below the latest version
fn bumps(text: &str, target: Version) -> HashSet<Bump> {
    let mut bumps: HashSet<Bump> = HashSet::new();
    for pattern in PATTERNS {
        for pin in all_pins(text, pattern.prefix)
            .iter()
            .filter(|p| p.version < target)
        {
            _ = bumps.insert(Bump {
                from: *pin,
                to: target,
            });
        }
    }
    bumps
}

/// Replaces every pin older than `target`. Splices back to front so earlier
/// offsets stay valid, and never walks a pin newer than `target` backwards.
fn update(text: &str, bumps: HashSet<Bump>) -> String {
    let mut out = text.to_string();
    for bump in bumps {
        let new_ver = bump.to.to_string();
        out.replace_range(bump.from.start..bump.from.end, &new_ver);
    }
    out
}

/// Routes `log` records to GitHub's workflow-command syntax.
///
/// No crate does this without dragging in unrelated dependencies, so this is
/// the same `env_logger` formatter approach `ghactions-core` takes, minus its
/// serde/indexmap/time tree, and with the prefixes spelled exactly as
/// documented -- `::warning::msg`, not `::warning :: msg`.
///
/// The target must be stdout. The runner only parses workflow commands there,
/// so a logger left on stderr silently stops annotating.
fn init_logger() {
    use std::io::Write;

    let level = if env::var_os("RUNNER_DEBUG").is_some() || env::var_os("DEBUG").is_some() {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };

    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Stdout)
        .filter_level(level)
        .format(|buf, record| match record.level() {
            // Warn and Error surface as annotations on the run and the diff.
            log::Level::Error => writeln!(buf, "::error::{}", record.args()),
            log::Level::Warn => writeln!(buf, "::warning::{}", record.args()),
            // Info is the running commentary, so leave it unadorned.
            log::Level::Info => writeln!(buf, "{}", record.args()),
            // Hidden unless the job is re-run with debug logging enabled.
            _ => writeln!(buf, "::debug::{}", record.args()),
        })
        .init();
}

/// Emits `::error::` and exits. `log::error!` alone cannot do this, and the
/// diverging return type is what lets it sit inside `unwrap_or_else`.
fn fail(msg: &str) -> ! {
    // Fall back to a bare print if this is reached before the logger is set up,
    // which would otherwise exit silently.
    if log::log_enabled!(log::Level::Error) {
        log::error!("{msg}");
    } else {
        println!("::error::{msg}");
    }
    std::process::exit(1);
}

/// Writes a step output using the heredoc form so multi-line values survive.
fn emit(name: &str, value: &str) {
    let Ok(path) = env::var("GITHUB_OUTPUT") else {
        return;
    };
    let Ok(mut file) = fs::OpenOptions::new().append(true).create(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{name}<<__NIX_BUMP_EOF__\n{value}\n__NIX_BUMP_EOF__");
}

/// Recursive walk for `*.nix` files. Symlinks are not followed, which keeps a
/// `result` build symlink from dragging the walk into the Nix store.
fn nix_files(dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "nix"))
        .map(walkdir::DirEntry::into_path)
        .collect()
}

/// Expands the requested paths into a deduplicated, sorted file list.
///
/// A directory is searched recursively for `*.nix`. A file is taken as given,
/// extension and all, because naming a file explicitly is the caller being
/// deliberate. A path that does not exist is fatal rather than a warning: a
/// typo that silently bumps nothing would leave a scheduled job green while
/// quietly falling behind upstream.
fn resolve_paths(paths: &[PathBuf]) -> HashSet<PathBuf> {
    let mut out: HashSet<PathBuf> = HashSet::new();
    for path in paths {
        if path.is_dir() {
            out.extend(nix_files(path));
        } else if path.is_file() {
            out.insert(path.clone());
        } else {
            fail(&format!("path '{}' does not exist", path.display()));
        }
    }
    out
}

/// get contents for each file
fn load_files(files: HashSet<PathBuf>) -> Vec<(PathBuf, String)> {
    let out = files
        .into_iter()
        .filter_map(|path| match fs::read_to_string(&path) {
            Ok(text) => Some((path, text)),
            // Skip anything that is not UTF-8 rather than abort the run.
            Err(e) => {
                log::warn!("skipping {}: {e}", path.display());
                None
            }
        })
        .collect();
    out
}

/// Versions among `refs/heads/<prefix><version>` ref names, ascending. The
fn versions_from_refs<'a>(refs: impl Iterator<Item = &'a str>, prefix: &str) -> HashSet<Version> {
    let want = format!("refs/heads/{prefix}");
    // strip so we get only the XX.YY from branch
    refs.filter_map(|name| Version::parse(name.strip_prefix(&want)?.get(..5)?))
        .collect()
}

// usge git to list all branches with a matching prefix
fn list_branches(repo: &str, prefix: &str) -> HashSet<String> {
    // The identity insteadOf is deliberate. A caller with a global
    // `url.ssh://git@github.com.insteadOf = https://github.com` rewrite would
    // otherwise turn this into an SSH fetch and fail without keys. Git prefers
    // the longest matching insteadOf, and this one is longer by the slash.
    let url = format!("https://github.com/{repo}");
    let output = Command::new("git")
        .args([
            "-c",
            "url.https://github.com/.insteadOf=https://github.com/",
            "ls-remote",
            "--heads",
            &url,
            &format!("refs/heads/{prefix}*"),
        ])
        .output()
        .unwrap_or_else(|e| fail(&format!("could not run git: {e}")));

    if !output.status.success() {
        fail(&format!(
            "git ls-remote {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    ref_names(&String::from_utf8_lossy(&output.stdout))
}

/// Ref names from `git ls-remote` stdout, whose lines are `<sha>\t<ref>`. Lines
/// without a tab are not refs and are skipped.
fn ref_names(stdout: &str) -> HashSet<String> {
    stdout
        .lines()
        .filter_map(|line| line.rsplit_once('\t'))
        .map(|(_, name)| name.to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

// return the latest release for the repo
fn latest_release(repo: &str, prefix: &str) -> Version {
    // get the list of branches for this repo with the prefix
    let mut versions: Vec<Version> = versions_from_refs(
        list_branches(repo, prefix).iter().map(String::as_str),
        prefix,
    )
    .into_iter()
    .collect();
    // sort and return latest version
    versions.sort();
    versions
        .pop()
        .unwrap_or_else(|| fail(&format!("found no {prefix}XX.YY branches in {repo}")))
}

/// Lets clap reject a malformed --target-version at parse time, with a usage
/// message, instead of us hand-checking it after the walk has already run.
fn parse_version(s: &str) -> Result<Version, String> {
    Version::parse(s).ok_or_else(|| format!("'{s}' is not of the form XX.YY"))
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Bump nixpkgs and home-manager release pins in *.nix files to the newest NixOS release",
    long_about = None,
)]
struct Args {
    /// Files and directories to bump. Directories are searched recursively for
    /// *.nix; files are taken as given. Defaults to the current directory.
    #[arg(value_name = "PATHS", default_value = ".")]
    paths: Vec<PathBuf>,

    /// Bump to this release (XX.YY) instead of discovering the newest one.
    /// Skips both upstream lookups.
    // No `short`: -t is free, but -d would collide with --dry-run's -n
    // neighbours and shorts on the repo flags would fight -h/--help.
    #[arg(short = 'v', long, value_parser = parse_version)]
    target_version: Option<Version>,

    /// Report what would change without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Repository to read release-* branches from.
    #[arg(
        long,
        default_value = "nix-community/home-manager",
        value_name = "OWNER/REPO"
    )]
    home_manager_repo: String,
}

fn main() {
    init_logger();
    let args = Args::parse();
    if let Err(e) = run(&args) {
        fail(&e.to_string());
    }
}

fn run(args: &Args) -> Result<Vec<PathBuf>> {
    let files = resolve_paths(&args.paths);

    if files.is_empty() {
        // resolve_paths already failed on any missing path, so an empty scan
        // means every requested path exists but yielded no *.nix files. That is
        // not a typo, just a tree with nothing to bump: report a no-op.
        log::info!("No *.nix files in the requested paths.");
        emit("bumped", "false");
        emit("files-changed", "");
        emit("versions-bumped", "");
        return Ok(Vec::new());
    }
    log::info!("Scanning {} *.nix file(s).", files.len());

    // home-manager only cuts release-XX.YY after nixpkgs cuts nixos-XX.YY, so
    // its newest release is always <= nixpkgs'. Using it directly is therefore
    // already the "newest release both have branched" that the old two-lookup
    // gate computed, and it cannot run ahead of nixpkgs by construction.
    let target_version = if let Some(v) = args.target_version {
        log::info!("Using pinned target {v}; skipping upstream discovery.");
        v
    } else {
        let v = latest_release(&args.home_manager_repo, "release-");
        log::info!("Upstream: home-manager release-{v}.");
        v
    };
    log::info!("Target release: {target_version}.");

    let mut changed: Vec<PathBuf> = Vec::new();
    let mut old_versions: HashSet<Version> = HashSet::new();

    emit("latest-home-manager", &target_version.to_string());
    emit("target", &target_version.to_string());

    // for each nix file, update to target version and rewrite
    for (path, text) in load_files(files) {
        let file_bumps = bumps(&text, target_version);
        old_versions.extend(file_bumps.iter().map(|b| b.from.version));
        let out = update(&text, file_bumps);
        // A file whose pins are all current or newer comes back byte-identical,
        // so it counts as untouched: it is neither written nor reported.
        if out == text {
            continue;
        }
        if !args.dry_run {
            if let Err(e) = fs::write(&path, out) {
                bail!("could not write {}: {e}", path.display());
            }
        }
        changed.push(path.clone());
    }

    if changed.is_empty() {
        log::info!("No pins to bump...");
        emit("bumped", "false");
        emit("files-changed", "");
        emit("versions-bumped", "");
        return Ok(changed);
    }

    let files_changed = changed
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let mut versions_bumped: Vec<String> = old_versions.iter().map(ToString::to_string).collect();
    versions_bumped.sort();

    emit("bumped", "true");
    emit("files-changed", &files_changed);
    emit("versions-bumped", &versions_bumped.join(","));

    if args.dry_run {
        log::info!("Dry run: would rewrite {} file(s).", changed.len());
        return Ok(changed);
    }

    log::info!("Rewrote {} file(s).", changed.len());
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).expect("test version should parse")
    }

    #[test]
    fn version_parse() {
        let cases = [
            ("25.05", Some(Version { year: 25, month: 5 })),
            (
                "26.11",
                Some(Version {
                    year: 26,
                    month: 11,
                }),
            ),
            ("00.00", Some(Version { year: 0, month: 0 })),
            ("25.05-small", None),
            ("2.05", None),
            ("25.5", None),
            ("25-05", None),
            ("aa.bb", None),
            ("", None),
            ("unstable", None),
        ];
        for (input, want) in cases {
            assert_eq!(Version::parse(input), want, "input {input:?}");
        }
    }

    #[test]
    fn version_display() {
        let cases = [
            (Version { year: 25, month: 5 }, "25.05"),
            (
                Version {
                    year: 26,
                    month: 11,
                },
                "26.11",
            ),
            (Version { year: 7, month: 5 }, "07.05"),
        ];
        for (input, want) in cases {
            assert_eq!(input.to_string(), want);
        }
    }

    #[test]
    fn version_ord() {
        let cases = [
            ("25.05", "25.11", true),
            ("25.11", "26.05", true),
            ("26.05", "26.05", false),
            ("26.11", "26.05", false),
            ("26.05", "25.11", false),
        ];
        for (a, b, want_lt) in cases {
            assert_eq!(v(a) < v(b), want_lt, "{a} < {b}");
        }
    }

    #[test]
    fn all_pins_locates_versions() {
        let prefix = "github:nixos/nixpkgs/nixos-";
        let pin_at = |text: &str, ver: &str| -> Pin {
            let start = text.find(prefix).unwrap() + prefix.len();
            Pin {
                start,
                end: start + 5,
                version: v(ver),
            }
        };
        let basic = "\"github:nixos/nixpkgs/nixos-25.05\"";
        let suffixed = "\"github:nixos/nixpkgs/nixos-25.05-small\"";
        let single = "a = \"github:nixos/nixpkgs/nixos-24.05\";";
        let cases: Vec<(&str, HashSet<Pin>)> = vec![
            (basic, HashSet::from([pin_at(basic, "25.05")])),
            (suffixed, HashSet::from([pin_at(suffixed, "25.05")])),
            (single, HashSet::from([pin_at(single, "24.05")])),
            // A channel with no version is ignored.
            ("\"github:nixos/nixpkgs/nixos-unstable\"", HashSet::new()),
            // Matching is case-sensitive, so a capitalised owner is not a pin.
            ("\"github:NixOS/nixpkgs/nixos-26.11\"", HashSet::new()),
            ("nothing to see here", HashSet::new()),
        ];
        for (input, want) in cases {
            assert_eq!(all_pins(input, prefix), want, "input {input:?}");
        }
    }

    #[test]
    fn versions_from_refs_includes_suffixed_refs() {
        let cases: [(&[&str], &str, HashSet<Version>); 3] = [
            // refs/heads/nixos-25.05-small reads the same 25.05 as its plain
            // sibling, while a versionless branch like nixos-unstable is
            // skipped.
            (
                &[
                    "refs/heads/nixos-25.05",
                    "refs/heads/nixos-25.05-small",
                    "refs/heads/nixos-26.05",
                    "refs/heads/nixos-unstable",
                ],
                "nixos-",
                HashSet::from([v("25.05"), v("26.05")]),
            ),
            (
                &["refs/heads/release-24.11", "refs/heads/release-25.05"],
                "release-",
                HashSet::from([v("24.11"), v("25.05")]),
            ),
            (&[], "nixos-", HashSet::new()),
        ];
        for (refs, prefix, want) in cases {
            assert_eq!(
                versions_from_refs(refs.iter().copied(), prefix),
                want,
                "prefix {prefix}"
            );
        }
    }

    #[test]
    fn ref_names_extracts_refs_from_ls_remote_lines() {
        let out = "d6602ec5194c87b0fc87103ca4c6726cfaadf2e0\trefs/heads/release-24.11\n\
                   d6602ec5194c87b0fc87103ca4c6726cfaadf2e0\trefs/heads/release-25.05\n";
        assert_eq!(
            ref_names(out),
            HashSet::from([
                "refs/heads/release-24.11".to_string(),
                "refs/heads/release-25.05".to_string(),
            ])
        );
    }

    #[test]
    fn resolve_paths_expands_dedups_and_sorts() {
        let root = std::env::temp_dir().join("nix-bump-resolve-paths-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        for (rel, body) in [
            ("a.nix", "a"),
            ("sub/b.nix", "b"),
            ("sub/notnix.txt", "t"),
            ("named.conf", "c"),
        ] {
            fs::write(root.join(rel), body).unwrap();
        }

        let cases: Vec<(Vec<PathBuf>, HashSet<PathBuf>)> = vec![
            // A directory walks recursively and skips non-.nix.
            (
                vec![root.clone()],
                HashSet::from([root.join("a.nix"), root.join("sub/b.nix")]),
            ),
            // A named file is honoured whatever its extension.
            (
                vec![root.join("named.conf")],
                HashSet::from([root.join("named.conf")]),
            ),
            // A file already covered by a listed directory appears once.
            (
                vec![root.clone(), root.join("a.nix")],
                HashSet::from([root.join("a.nix"), root.join("sub/b.nix")]),
            ),
            // Repeated paths collapse, and output is sorted.
            (
                vec![
                    root.join("sub/b.nix"),
                    root.join("a.nix"),
                    root.join("a.nix"),
                ],
                HashSet::from([root.join("a.nix"), root.join("sub/b.nix")]),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(resolve_paths(&input), want, "input {input:?}");
        }

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bumps_only_stale_pins() {
        let target = v("26.05");
        let text = "- \"github:nixos/nixpkgs/nixos-20.09\"\n\
                     - \"github:nixos/nixpkgs/nixos-26.05\"\n\
                     - \"github:nixos/nixpkgs/nixos-99.11\"\n\
                     - \"github:nix-community/home-manager/release-25.11\"\n";
        let bumps = bumps(text, target);
        let mut from: Vec<Version> = bumps.iter().map(|b| b.from.version).collect();
        from.sort();
        assert_eq!(from, vec![v("20.09"), v("25.11")]);
        assert!(
            bumps
                .iter()
                .all(|b| b.to == target && b.from.end - b.from.start == 5),
            "bump targets and spans"
        );
    }

    #[test]
    fn update_replaces_stale_but_keeps_others() {
        let target = v("26.05");
        let cases: [(&str, &str); 4] = [
            // Stale pins move; current, newer, suffixed and versionless
            // channels, mixed-case owners and non-pattern text are all left
            // alone.
            (
                "a = \"github:nixos/nixpkgs/nixos-20.09\"\n\
                 b = \"github:nixos/nixpkgs/nixos-26.05\"\n\
                 c = \"github:nixos/nixpkgs/nixos-99.11\"\n\
                 d = \"github:nixos/nixpkgs/nixos-21.11-small\"\n\
                 e = \"github:NixOS/nixpkgs/nixos-20.09\"\n\
                 system.stateVersion = \"20.09\";",
                "a = \"github:nixos/nixpkgs/nixos-26.05\"\n\
                 b = \"github:nixos/nixpkgs/nixos-26.05\"\n\
                 c = \"github:nixos/nixpkgs/nixos-99.11\"\n\
                 d = \"github:nixos/nixpkgs/nixos-26.05-small\"\n\
                 e = \"github:NixOS/nixpkgs/nixos-20.09\"\n\
                 system.stateVersion = \"20.09\";",
            ),
            // nixos-unstable is versionless, so it is skipped untouched.
            (
                "{ x = \"github:nixos/nixpkgs/nixos-unstable\"; }",
                "{ x = \"github:nixos/nixpkgs/nixos-unstable\"; }",
            ),
            // Several stale pins on one line all move.
            (
                "[ \"github:nixos/nixpkgs/nixos-24.05\" \"github:nixos/nixpkgs/nixos-25.11\" ]",
                "[ \"github:nixos/nixpkgs/nixos-26.05\" \"github:nixos/nixpkgs/nixos-26.05\" ]",
            ),
            // Nothing stale: the text comes back unchanged.
            (
                "a = \"github:nixos/nixpkgs/nixos-26.05\";",
                "a = \"github:nixos/nixpkgs/nixos-26.05\";",
            ),
        ];
        for (input, want) in cases {
            let out = update(input, bumps(input, target));
            assert_eq!(out, want, "input {input:?}");
        }
    }

    #[test]
    fn small_suffix_pins_bump_with_suffix_kept() {
        // refs/heads/nixos-XX.YY-small exist upstream and read the same
        // version as the plain branch, so a small-profile pin moves with its
        // suffix preserved. Only the five version bytes change.
        let target = v("26.05");
        let stale = "a = \"github:nixos/nixpkgs/nixos-21.11-small\";";
        assert_eq!(
            update(stale, bumps(stale, target)),
            "a = \"github:nixos/nixpkgs/nixos-26.05-small\";"
        );
        // Already at the target: the suffix stays and nothing moves.
        let current = "a = \"github:nixos/nixpkgs/nixos-26.05-small\";";
        assert_eq!(update(current, bumps(current, target)), current);
    }

    #[test]
    fn parse_version_check_shapes() {
        assert_eq!(parse_version("25.05"), Ok(v("25.05")));
        assert_eq!(parse_version("26.11"), Ok(v("26.11")));
        for bad in ["2.05", "25.5", "25.05-small", "", "unstable"] {
            assert!(parse_version(bad).is_err(), "input {bad:?}");
        }
    }

    #[test]
    fn nix_files_finds_only_nix_recursively() {
        let root = std::env::temp_dir().join("nix-bump-nix-files-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.nix"), "").unwrap();
        fs::write(root.join("sub/b.nix"), "").unwrap();
        fs::write(root.join("sub/c.txt"), "").unwrap();
        fs::create_dir_all(root.join("sub/dir.nix")).unwrap();
        let mut got = nix_files(&root);
        got.sort();
        assert_eq!(got, vec![root.join("a.nix"), root.join("sub/b.nix")]);
        // A file path is returned as-is.
        assert_eq!(nix_files(&root.join("a.nix")), vec![root.join("a.nix")]);
    }

    #[test]
    fn load_files_reads_utf8_and_skips_others() {
        let root = std::env::temp_dir().join("nix-bump-load-files-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let good = root.join("good.nix");
        let bad = root.join("bad.nix");
        fs::write(&good, "hello\n").unwrap();
        fs::write(&bad, b"\xff\xfe\x00").unwrap();
        let files = HashSet::from([good.clone(), bad.clone(), root.join("missing.nix")]);
        assert_eq!(load_files(files), vec![(good, "hello\n".to_string())]);
    }

    #[test]
    fn emit_writes_github_output_heredoc() {
        let path = std::env::temp_dir().join("nix-bump-emit-test");
        let _ = fs::remove_file(&path);
        env::set_var("GITHUB_OUTPUT", &path);
        emit("bumped", "true");
        emit("files-changed", "a.nix\nb.nix");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "bumped<<__NIX_BUMP_EOF__\ntrue\n__NIX_BUMP_EOF__\n\
             files-changed<<__NIX_BUMP_EOF__\na.nix\nb.nix\n__NIX_BUMP_EOF__\n"
        );
        // No GITHUB_OUTPUT: silently does nothing.
        env::remove_var("GITHUB_OUTPUT");
        emit("bumped", "false");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn run_bumps_pinned_target() {
        // A pinned target means the upstream git lookup is skipped entirely,
        // so the test never touches the network.
        let root = std::env::temp_dir().join("nix-bump-run-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(
            root.join("bumped.nix"),
            "{ inputs = {\n    nixpkgs.url = \"github:nixos/nixpkgs/nixos-24.11\";\n    home-manager.url = \"github:nix-community/home-manager/release-24.11\";\n  };\n}",
        )
        .unwrap();
        fs::write(
            root.join("up-to-date.nix"),
            "{ inputs.nixpkgs.url = \"github:nixos/nixpkgs/nixos-26.05\"; }",
        )
        .unwrap();
        fs::write(
            root.join("sub/extra.nix"),
            "{ inputs.nixpkgs.url = \"github:nixos/nixpkgs/nixos-20.09\"; }",
        )
        .unwrap();

        let out = std::env::temp_dir().join("nix-bump-run-test-output");
        let _ = fs::remove_file(&out);
        env::set_var("GITHUB_OUTPUT", &out);
        let args = Args {
            paths: vec![root.clone()],
            target_version: Some(v("26.05")),
            dry_run: false,
            home_manager_repo: "nix-community/home-manager".to_string(),
        };
        let changed = run(&args).unwrap();

        // Only the two files with stale pins are rewritten; the up-to-date file
        // is untouched and not reported.
        let mut got = changed.clone();
        got.sort();
        assert_eq!(
            got,
            vec![root.join("bumped.nix"), root.join("sub/extra.nix")]
        );

        // Stale pins move to the target, the up-to-date file is untouched.
        let up_to_date = fs::read_to_string(root.join("up-to-date.nix")).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("bumped.nix")).unwrap(),
            "{ inputs = {\n    nixpkgs.url = \"github:nixos/nixpkgs/nixos-26.05\";\n    home-manager.url = \"github:nix-community/home-manager/release-26.05\";\n  };\n}"
        );
        assert_eq!(
            fs::read_to_string(root.join("sub/extra.nix")).unwrap(),
            "{ inputs.nixpkgs.url = \"github:nixos/nixpkgs/nixos-26.05\"; }"
        );
        assert!(matches!(
            up_to_date.trim(),
            "{ inputs.nixpkgs.url = \"github:nixos/nixpkgs/nixos-26.05\"; }"
        ));

        // The heredoc output reports the pinned target and the rewrite.
        let output = fs::read_to_string(&out).unwrap();
        assert!(output.contains("target<<__NIX_BUMP_EOF__\n26.05\n__NIX_BUMP_EOF__"));
        assert!(output.contains("latest-home-manager<<__NIX_BUMP_EOF__\n26.05\n__NIX_BUMP_EOF__"));
        assert!(output.contains("bumped<<__NIX_BUMP_EOF__\ntrue\n__NIX_BUMP_EOF__"));
        assert!(output.contains("versions-bumped<<__NIX_BUMP_EOF__\n20.09,24.11\n__NIX_BUMP_EOF__"));
        for path in &changed {
            assert!(
                output.contains(&path.display().to_string()),
                "files-changed should list {}",
                path.display()
            );
        }

        env::remove_var("GITHUB_OUTPUT");
        let _ = fs::remove_file(&out);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn run_bumps_pinned_target_reports_noop_for_nixless_tree() {
        // A directory that exists but holds no *.nix files is a no-op, not an
        // error: typed paths were already proven to exist by resolve_paths.
        let root = std::env::temp_dir().join("nix-bump-empty-run-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("readme.md"), "no nix here").unwrap();

        let out = std::env::temp_dir().join("nix-bump-empty-run-test-output");
        let _ = fs::remove_file(&out);
        env::set_var("GITHUB_OUTPUT", &out);
        let args = Args {
            paths: vec![root.clone()],
            target_version: Some(v("26.05")),
            dry_run: false,
            home_manager_repo: "nix-community/home-manager".to_string(),
        };
        let changed = run(&args).unwrap();
        assert!(changed.is_empty());

        let output = fs::read_to_string(&out).unwrap();
        assert!(output.contains("bumped<<__NIX_BUMP_EOF__\nfalse\n__NIX_BUMP_EOF__"));

        env::remove_var("GITHUB_OUTPUT");
        let _ = fs::remove_file(&out);
        fs::remove_dir_all(&root).unwrap();
    }
}
