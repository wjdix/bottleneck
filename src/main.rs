//! bottleneck — find where PRs get stuck.

mod config;
mod domain;
mod github;

use anyhow::Result;
use clap::Parser;

use config::Config;
use domain::trunk::Trunk;
use github::{GitHub, fetch};

#[derive(Parser, Debug)]
#[command(name = "bottleneck", about = "Find where PRs get stuck")]
struct Args {
    /// Print parsed deploy tags for each configured env and exit.
    #[arg(long)]
    dump_tags: bool,

    /// Print the reconstructed first-parent trunk (oldest last) and exit.
    ///
    /// Compare against `git log --first-parent --format=%H` on a clone; they must match exactly.
    #[arg(long)]
    dump_trunk: bool,

    /// Print resolved deploy state per env for recent merged PRs and exit.
    #[arg(long)]
    dump_deploys: bool,

    /// Override the history window, in days.
    #[arg(long)]
    days: Option<i64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut cfg = Config::load(&config::config_path()?)?;
    if let Some(d) = args.days {
        cfg.window_days = d;
    }

    if args.dump_tags {
        return dump_tags(&cfg).await;
    }
    if args.dump_trunk {
        return dump_trunk(&cfg).await;
    }

    println!("TUI not implemented yet — try --dump-tags");
    Ok(())
}

/// Stage-1 verification path: exercises auth, the GraphQL client, paging, and tag parsing
/// end-to-end against the live repo before any UI exists.
async fn dump_tags(cfg: &Config) -> Result<()> {
    let gh = GitHub::new(GitHub::resolve_token().await?)?;
    let since = cfg.window_start();

    for repo in &cfg.repos {
        println!("=== {} (tags with commits since {}) ===", repo.slug(), since.date_naive());
        for env in &cfg.envs {
            let scan = fetch::fetch_tags(&gh, repo, env, since).await?;
            println!(
                "  {env}: {} deploy tags, {} unparseable, {} without commit",
                scan.tags.len(),
                scan.unparseable.len(),
                scan.no_commit.len()
            );
            for tag in scan.tags.iter().take(3) {
                println!(
                    "    {:<44} {} {}",
                    tag.name,
                    &tag.commit_oid[..8.min(tag.commit_oid.len())],
                    tag.commit_date.to_rfc3339()
                );
            }
            for name in &scan.unparseable {
                println!("    skipped (unparseable): {name}");
            }
        }
    }
    Ok(())
}

/// Stage-3 verification path: prints the reconstructed first-parent trunk for diffing against
/// `git log --first-parent`.
async fn dump_trunk(cfg: &Config) -> Result<()> {
    let gh = GitHub::new(GitHub::resolve_token().await?)?;
    let since = cfg.window_start();

    for repo in &cfg.repos {
        let scan = fetch::fetch_history(&gh, repo, since).await?;
        let parents = scan
            .commits
            .iter()
            .map(|c| (c.oid.clone(), c.first_parent.clone()))
            .collect();
        let trunk = Trunk::build(&scan.head, &parents);

        eprintln!(
            "{}: fetched {} commits in {} pages; trunk {} commits (truncated at window edge: {})",
            repo.slug(),
            scan.commits.len(),
            scan.pages,
            trunk.len(),
            trunk.truncated_at_window_edge
        );

        // Print in trunk order, index 0 (head) first, so it lines up with `git log --first-parent`.
        let mut ordered: Vec<(usize, &str)> = scan
            .commits
            .iter()
            .filter_map(|c| trunk.index_of(&c.oid).map(|i| (i, c.oid.as_str())))
            .collect();
        ordered.sort_by_key(|(i, _)| *i);
        for (_, oid) in ordered {
            println!("{oid}");
        }
    }
    Ok(())
}
