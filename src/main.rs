use anyhow::{format_err, Context, Result};
use cargo_new_release::CommandExt;
use dialoguer::Confirm;
use regex::Regex;
use semver::Version;
use std::env;
use std::fs;
use std::process::{exit, Command};

/// Checks that the repo is ready to go.
fn check_status() -> Result<()> {
    let root = Command::git("rev-parse --show-toplevel").run_stdout()?;
    env::set_current_dir(root)?;
    if !Command::git("diff-index --quiet HEAD .").run_success()? {
        eprintln!("Working tree has changes.");
        Command::git("status --porcelain").run_success()?;
        if !Confirm::new()
            .with_prompt("Do you want to continue?")
            .default(false)
            .interact()?
        {
            exit(1);
        }
    }
    // Check repo looks correct.
    let upstream = Command::git("config remote.upstream.url").run_stdout()?;
    if !upstream.ends_with("rust-lang/cargo.git") {
        eprintln!(
            "error: upstream does not appear to be rust-lang/cargo, was: {}",
            upstream
        );
        exit(1);
    }
    let origin = Command::git("config remote.origin.url").run_stdout()?;
    if !origin.ends_with("/cargo.git") {
        eprintln!("error: origin does not appear to be cargo, was: {}", origin);
        exit(1);
    }
    Ok(())
}

/// Creates the `version-bump` branch.
fn create_branch() -> Result<()> {
    if !Command::git("fetch upstream --tags").run_success()? {
        eprintln!("error: failed to fetch upstream");
        exit(1);
    }
    // Check if branch exists, and delete it if it does.
    if Command::git("show-ref --verify --quiet refs/heads/version-bump").run_success()? {
        eprintln!("info: removing version-bump branch");
    }
    eprintln!("info: creating version-bump branch");
    if !Command::git("checkout -B version-bump upstream/master").run_success()? {
        eprintln!("error: failed to create branch");
        exit(1);
    }
    if !Command::git("config branch.version-bump.remote origin").run_success()? {
        eprintln!("error: failed to set remote origin");
        exit(1);
    }
    if !Command::git("config branch.version-bump.merge refs/heads/version-bump").run_success()? {
        eprintln!("error: failed to set branch merge");
        exit(1);
    }
    Ok(())
}

/// Updates the version in `Cargo.toml`.
fn bump_version_toml() -> Result<Version> {
    // TODO: run some validation if dependent crates like crates-io need to be updated.
    let mut toml = fs::read_to_string("Cargo.toml")
        .with_context(|| format_err!("failed to read Cargo.toml"))?;
    let version_start = toml.find("version = \"").expect("version") + 11;
    let len = toml[version_start..].find('"').expect("version end");
    let version = Version::parse(&toml[version_start..version_start + len]).expect("valid version");
    assert_eq!(version.major, 0);
    let next_version = Version::new(0, version.minor + 1, 0);
    toml.replace_range(
        version_start..version_start + len,
        &next_version.to_string(),
    );
    fs::write("Cargo.toml", toml)?;
    Ok(next_version)
}

/// Waits for the user to manually validate.
fn wait_for_inspection() -> Result<()> {
    eprintln!("Check for any tests or rustc probing (usually target_info.rs) that can be updated.");
    if !Confirm::new()
        .with_prompt("Ready to commit?")
        .default(true)
        .interact()?
    {
        exit(1);
    }
    Ok(())
}

/// Commits the version bump.
fn commit_bump(next_version: &Version) -> Result<()> {
    if !Command::git("commit -a -m")
        .arg(format!("Bump to {}", next_version))
        .run_success()?
    {
        eprintln!("error: failed to commit");
        exit(1);
    }
    Ok(())
}

/// Modifies `CHANGELOG.md` to include stubs for the given version.
fn prep_changelog(next_version: &Version, rust_repo: &str) -> Result<()> {
    let beta_minor_version = next_version.minor - 2;
    // Determine the version in rust-lang/rust beta branch.
    if !Command::git("fetch upstream --tags")
        .current_dir(rust_repo)
        .run_success()?
    {
        eprintln!("error: failed to fetch rust upstream");
        exit(1);
    }
    let last_beta_line = Command::git("ls-tree upstream/beta src/tools/cargo")
        .current_dir(rust_repo)
        .run_stdout()?;
    let mut parts = last_beta_line.split_whitespace();
    assert_eq!(parts.next(), Some("160000"));
    assert_eq!(parts.next(), Some("commit"));
    let last_beta_hash = parts.next().expect("hash");
    assert_eq!(parts.next(), Some("src/tools/cargo"));

    // Determine the rust-lang/cargo beta version.
    let last_branch_line = Command::git(&format!(
        "show-ref upstream/rust-1.{}.0",
        beta_minor_version
    ))
    .run_stdout()?;
    let last_branch_hash = last_branch_line.split_whitespace().next().expect("hash");

    if last_beta_hash != last_branch_hash {
        eprintln!(
            "warning: rust-lang/rust beta branch hash {} does not equal \
            rust-lang/cargo upstream/rust-1.{}.0 hash {}",
            last_beta_hash, beta_minor_version, last_branch_hash
        );
        eprintln!(
            "This may happen if changes are pushed to rust-1.{}.0 shortly after the beta \
             branch was created. Please carefully inspect to verify that this is the case.
            ",
            beta_minor_version
        );
        if !Confirm::new()
            .with_prompt("Do you want to continue?")
            .default(true)
            .interact()?
        {
            exit(1);
        }
    }
    let start_of_beta_short_hash = &last_beta_hash[..8];

    let to_links = |prs: &[(u32, String, String)]| -> String {
        prs.iter()
            .map(|(num, url, descr)| format!("- {} \n  [#{}]({})\n", descr, num, url))
            .collect::<Vec<_>>()
            .join("")
    };

    // Update last version.
    let changelog = fs::read_to_string("CHANGELOG.md")
        .with_context(|| format_err!("failed to read CHANGELOG.md"))?;

    let head_re = Regex::new(r"([a-f0-9]+)\.\.\.HEAD").unwrap();
    let matches: Vec<_> = head_re.captures_iter(&changelog).collect();
    assert_eq!(matches.len(), 2);
    assert_eq!(
        matches[0].get(0).unwrap().as_str(),
        matches[1].get(0).unwrap().as_str()
    );
    let beta_hash_start = matches[0].get(1).unwrap().as_str();
    let beta_version = format!("rust-1.{}.0", beta_minor_version);
    let mut changelog = head_re
        .replace_all(
            &changelog,
            format!("{}...{}", beta_hash_start, beta_version).as_str(),
        )
        .into_owned();

    // Determine changes in master (nightly).
    let master_prs = find_prs(&changelog, start_of_beta_short_hash, "upstream/master")?;
    // Determine changes in beta.
    let beta_prs = find_prs(
        &changelog,
        beta_hash_start,
        &format!("upstream/{}", beta_version),
    )?;

    let added_idx = changelog.find("### Added\n").expect("couldn't find added");
    changelog.insert_str(added_idx, &to_links(&beta_prs));

    // Insert new version.
    assert!(changelog.starts_with("# Changelog\n"));
    changelog.insert_str(
        12,
        &format!(
            "\n## Cargo 1.{} ({DATE})\n\
        [{HASH}...HEAD](https://github.com/rust-lang/cargo/compare/{HASH}...HEAD)\n\
        \n\
        {LINKS}\n\
        \n\
        ### Added\n\
        \n\
        ### Changed\n\
        \n\
        ### Fixed\n\
        \n\
        ### Nightly only\n\
        \n\
        ",
            next_version.minor - 1,
            HASH = start_of_beta_short_hash,
            LINKS = to_links(&master_prs),
            DATE = next_version_date(next_version),
        ),
    );
    fs::write("CHANGELOG.md", changelog)?;

    let master_urls: Vec<_> = master_prs
        .iter()
        .map(|(_pr, url, _descr)| url.as_str())
        .collect();
    open_browser(&master_urls)?;

    eprintln!(
        "Update the nightly version 1.{}.0 and come back when finished.",
        next_version.minor - 1
    );
    if !Confirm::new()
        .with_prompt("Ready to continue?")
        .default(true)
        .interact()?
    {
        exit(1);
    }

    let beta_urls: Vec<_> = beta_prs
        .iter()
        .map(|(_pr, url, _descr)| url.as_str())
        .collect();
    open_browser(&beta_urls)?;

    eprintln!(
        "Update the beta version 1.{}.0 and come back when finished.",
        beta_minor_version
    );
    if !Confirm::new()
        .with_prompt("Ready to commit?")
        .default(true)
        .interact()?
    {
        exit(1);
    }

    Ok(())
}

fn open_browser(urls: &[&str]) -> Result<()> {
    if !Command::new("/Applications/Firefox.app/Contents/MacOS/firefox")
        .arg("-url")
        .args(urls)
        .run_success()?
    {
        eprintln!("error: failed to open firefox");
        exit(1);
    }
    Ok(())
}

fn find_prs(changelog: &str, start: &str, end: &str) -> Result<Vec<(u32, String, String)>> {
    let cmd = format!("log --first-parent {}...{}", start, end);
    let log = Command::git(&cmd).run_stdout()?;
    let commits = cargo_new_release::commits_in_log(&log);

    let (dupe, new): (Vec<_>, Vec<_>) = commits
        .into_iter()
        .partition(|(pr, _url, _descr)| changelog.contains(&format!("[#{}]", pr)));
    for (pr, _url, _descr) in dupe {
        eprintln!("skipping PR #{}, already documented", pr);
    }
    Ok(new)
}

/// Commits the changelog update.
fn commit_changelog(next_version: &Version) -> Result<()> {
    if !Command::git("commit -a -m")
        .arg(format!("Update changelog for 1.{}", next_version.minor - 2))
        .run_success()?
    {
        eprintln!("error: failed to commit changelog");
        exit(1);
    }
    Ok(())
}

/// Creates the PR.
fn create_pr(next_vers: &Version) -> Result<()> {
    if !Command::git("push").run_success()? {
        eprintln!("error: failed to push");
        exit(1);
    }
    let origin = Command::git("remote get-url origin").run_stdout()?;
    let user_re = Regex::new(r"([a-zA-Z0-9-]+)/cargo").unwrap();
    let user_cap = user_re.captures(&origin).expect("user in origin");
    let username = &user_cap[1];
    open_browser(&[&format!(
        "https://github.com/{username}/cargo/pull/new/version-bump"
    )])?;
    // TODO: Use github API (or maybe query-strings?) to set title
    eprintln!("title:\nBump to {}, update changelog", next_vers);
    Ok(())
}

fn next_version_date(next_vers: &Version) -> String {
    let first = time::date!(2015 - 05 - 15); // 1.0.0 release date
    let next_days = ((next_vers.minor - 1) * 42) as i64;
    let next_date = time::Date::from_julian_day(first.julian_day() + next_days - 1);
    next_date.format("%Y-%m-%d")
}

fn doit() -> Result<()> {
    let rust_repo = env::args()
        .skip(1)
        .next()
        .ok_or_else(|| format_err!("expected path to rust repo as first argument"))?;
    check_status()?;
    create_branch()?;
    let next_vers = bump_version_toml()?;
    wait_for_inspection()?;
    commit_bump(&next_vers)?;
    prep_changelog(&next_vers, &rust_repo)?;
    commit_changelog(&next_vers)?;
    create_pr(&next_vers)?;
    Ok(())
}

fn main() {
    if let Err(e) = doit() {
        eprintln!("error: {}", e);
        for cause in e.chain().skip(1) {
            eprintln!("caused by: {}", cause);
        }
    }
}
