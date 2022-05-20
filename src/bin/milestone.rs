use anyhow::{bail, format_err, Result};
use cargo_new_release::CommandExt;
use dialoguer::Confirm;
use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::process::exit;
use std::process::Command;

fn fetch(rust_repo: &Path) -> Result<()> {
    Command::git("fetch upstream")
        .current_dir(rust_repo)
        .run_success()?;
    Ok(())
}

/// Determine which PRs need to be milestoned.
fn determine_milestones(auth: &str, rust_repo: &Path) -> Result<HashMap<String, Vec<u32>>> {
    let log = Command::git("log --remotes=upstream -n 100 --format=%H src/tools/cargo")
        .current_dir(rust_repo)
        .run_stdout()?;
    let subproject_re = Regex::new("Subproject commit ([0-9a-f]+)").unwrap();
    let mut to_milestone = HashMap::new();
    for hash in log.lines() {
        let diff = Command::git(&format!("show -p {hash} src/tools/cargo"))
            .current_dir(rust_repo)
            .run_stdout()?;
        let mut caps = subproject_re.captures_iter(&diff);
        let cargo_start_hash = &caps.next().unwrap()[1];
        let cargo_end_hash = &caps.next().unwrap()[1];
        assert!(caps.next().is_none());
        let version = version_at(rust_repo, hash)?;
        let log = Command::git(&format!(
            "log --first-parent {cargo_start_hash}...{cargo_end_hash}"
        ))
        .current_dir(rust_repo.join("src/tools/cargo"))
        .run_stdout()?;
        let commits = cargo_new_release::commits_in_log(&log)?;
        assert!(!commits.is_empty());
        let mut found = false;
        for (pr_num, _, _) in commits {
            if let Some((_milestone_number, milestone_title)) = current_milestone(auth, pr_num)? {
                if milestone_title == version {
                    eprintln!("skipping PR {pr_num}, already milestoned to {version}");
                } else {
                    eprintln!("PR {pr_num} is already milestoned, but milestone {milestone_title:?} does not match version {version:?}");
                }
                continue;
            }
            let to_mile_prs: &mut Vec<u32> = to_milestone.entry(version.to_string()).or_default();
            to_mile_prs.push(pr_num);
            found = true;
        }
        if !found {
            break;
        }
    }
    Ok(to_milestone)
}

/// Determines the release version at the given git hash.
fn version_at(rust_repo: &Path, hash: &str) -> Result<String> {
    Command::git(&format!("show {hash}:src/version"))
        .current_dir(rust_repo)
        .run_stdout()
}

/// Returns the current milestone for the given PR.
///
/// Returns None if no milestone currently set.
/// Otherwise returns a tuple `(milestone_number, milestone_title)`.
fn current_milestone(auth: &str, pr_num: u32) -> Result<Option<(String, String)>> {
    let url = format!("https://api.github.com/repos/rust-lang/cargo/issues/{pr_num}");
    let response = match ureq::get(&url)
        .set("Accept", "application/vnd.github.v3+json")
        .set("Authorization", &format!("Basic {auth}"))
        .call()
    {
        Ok(r) => r,
        Err(e) => match e {
            ureq::Error::Status(status, response) => {
                let body = response.into_string().unwrap_or_default();
                bail!("{url} failed status {status}: {body}");
            }
            _ => {
                return Err(e.into());
            }
        },
    };
    let status = response.status();
    if status != 200 {
        let body = response.into_string().unwrap_or_default();
        bail!("failed response on PR {pr_num} {status} {body}");
    }
    let pr: serde_json::Value = response.into_json()?;
    let milestone = &pr["milestone"];
    if milestone.is_null() {
        return Ok(None);
    }
    let number = milestone["number"].to_string();
    let title = milestone["title"].as_str().unwrap().to_string();
    Ok(Some((number, title)))
}

/// Confirm to start milestoning.
fn confirm(milestones: &HashMap<String, Vec<u32>>) -> Result<()> {
    eprintln!("milestoning:");
    for (version, prs) in milestones {
        eprintln!("{version}");
        for pr in prs {
            eprintln!("    https://github.com/rust-lang/cargo/pull/{pr}");
        }
    }
    if !Confirm::new()
        .with_prompt("Ready to milestone?")
        .default(true)
        .interact()?
    {
        exit(1);
    }
    Ok(())
}

/// Sets the milestone for the given PRs.
fn set_milestones(auth: &str, milestones: &HashMap<String, Vec<u32>>) -> Result<()> {
    for (version, prs) in milestones {
        let milestone_num = get_milestone_num(auth, version)?;
        for pr in prs {
            eprintln!("updating pr {pr} to milestone {version} ({milestone_num})");
            let url = format!("https://api.github.com/repos/rust-lang/cargo/issues/{pr}");
            let response = ureq::patch(&url)
                .set("Accept", "application/vnd.github.v3+json")
                .set("Authorization", &format!("Basic {auth}"))
                .send_json(ureq::json!({
                    "milestone": milestone_num,
                }))?;
            if response.status() != 200 {
                bail!("failed response on PR {pr} {response:?}");
            }
        }
    }
    Ok(())
}

/// Returns the milestone number for the given release version.
///
/// Creates the milestone if it doesn't already exist.
fn get_milestone_num(auth: &str, version: &str) -> Result<i64> {
    // Create the milestone.
    let url = format!("https://api.github.com/repos/rust-lang/cargo/milestones");
    let number = match ureq::post(&url)
        .set("Accept", "application/vnd.github.v3+json")
        .set("Authorization", &format!("Basic {auth}"))
        .send_json(ureq::json!({
            "title": version,
            "state": "closed",
        })) {
        Ok(response) => {
            eprintln!("created milestone: {response:?}");
            let milestone_body: serde_json::Value = response.into_json()?;
            eprintln!("{:?}", milestone_body);
            milestone_body["number"].as_i64().unwrap()
        }
        Err(ureq::Error::Status(422, _response)) => {
            let milestones: serde_json::Value = ureq::get(&format!(
                "https://api.github.com/repos/rust-lang/cargo/milestones?state=all&per_page=100"
            ))
            .set("Accept", "application/vnd.github.v3+json")
            .set("Authorization", &format!("Basic {auth}"))
            .call()?
            .into_json()?;
            milestones
                .as_array()
                .unwrap()
                .into_iter()
                .find(|milestone| milestone["title"] == version)
                .map(|milestone| milestone["number"].as_i64().unwrap())
                .ok_or_else(|| format_err!("could not find {version}"))?
        }
        Err(e) => return Err(e.into()),
    };
    Ok(number)
}

fn doit() -> Result<()> {
    let rust_repo = env::args()
        .skip(1)
        .next()
        .ok_or_else(|| format_err!("expected path to rust repo as first argument"))?;
    let token = env::var("GITHUB_TOKEN").expect("GITHUB_TOKEN must be set");
    let auth = base64::encode(format!("ehuss:{token}"));
    let rust_repo = Path::new(&rust_repo);
    fetch(&rust_repo)?;
    let milestones = determine_milestones(&auth, &rust_repo)?;
    confirm(&milestones)?;
    set_milestones(&auth, &milestones)?;
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
