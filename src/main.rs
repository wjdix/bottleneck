//! bottleneck — find where PRs get stuck.

mod config;
mod domain;
mod github;
mod snapshot;
mod store;
mod sync;

use anyhow::Result;
use clap::Parser;

use config::Config;
use domain::metrics::format_duration;
use domain::trunk::{DeployState, Trunk};
use snapshot::Snapshot;
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
    if args.dump_deploys {
        return dump_deploys(&cfg).await;
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

/// Stage-3 verification path: prints the analysed snapshot so every headline number can be checked
/// against GitHub by hand before any UI exists.
async fn dump_deploys(cfg: &Config) -> Result<()> {
    let gh = GitHub::new(GitHub::resolve_token().await?)?;

    for repo in &cfg.repos {
        let snap = Snapshot::fetch(&gh, repo, cfg, |stage| eprintln!("  fetching {stage}...")).await?;

        println!("=== {} ===", repo.slug());
        println!(
            "window from {}  |  {} PRs  |  trunk {} commits  |  {} tags skipped",
            snap.window_start.date_naive(),
            snap.prs.len(),
            snap.trunk.len(),
            snap.skipped_tags.len()
        );

        println!("\n-- review latency --");
        for (label, s) in [
            ("draft dwell        ", snap.aggregates.draft_dwell),
            ("time to 1st review ", snap.aggregates.time_to_first_review),
            ("review turnaround  ", snap.aggregates.review_turnaround),
            ("time to merge      ", snap.aggregates.time_to_merge),
        ] {
            match s {
                Some(s) => println!(
                    "  {label} median {:>9}  p75 {:>9}  max {:>9}  (n={})",
                    format_duration(s.median),
                    format_duration(s.p75),
                    format_duration(s.max),
                    s.n
                ),
                None => println!("  {label} no data"),
            }
        }

        println!("\n-- deploy latency (envs are independent) --");
        for env in &snap.envs {
            let cov = snap.coverage.get(env).cloned().unwrap_or_default();
            print!(
                "  {env:<6} {:>3}/{:<4} tags on trunk ({:>3.0}%){}",
                cov.on_trunk,
                cov.total(),
                cov.fraction_on_trunk() * 100.0,
                if cov.is_partial() { " PARTIAL" } else { "        " }
            );
            match snap.aggregates.merge_to_deploy.get(env) {
                Some(s) => print!(
                    "  merge->deploy median {:>9} p75 {:>9} (n={})",
                    format_duration(s.median),
                    format_duration(s.p75),
                    s.n
                ),
                None => print!("  merge->deploy no data"),
            }
            match snap.aggregates.open_to_deploy.get(env) {
                Some(s) => print!(
                    "  |  open->deploy median {:>9} p75 {:>9} (n={})",
                    format_duration(s.median),
                    format_duration(s.p75),
                    s.n
                ),
                None => print!("  |  open->deploy no data"),
            }
            println!(
                "  |  awaiting deploy: {}",
                snap.aggregates.not_yet_deployed.get(env).unwrap_or(&0)
            );
        }
        println!("  stacked PRs excluded from deploy stats: {}", snap.aggregates.stacked);
        for env in &snap.envs {
            if let Some(cov) = snap.coverage.get(env)
                && cov.is_partial()
            {
                println!(
                    "  NOTE {env}: only {:.0}% of deploy tags are cut from the trunk; the rest are \
                     branch deploys this attribution cannot see, so {env} latency is overstated.",
                    cov.fraction_on_trunk() * 100.0
                );
            }
        }

        println!("\n-- threads of work (open PRs) --");
        let (mut open, mut draft) = (0, 0);
        for p in &snap.people {
            open += p.total();
            draft += p.draft;
        }
        println!("  TOTAL open={open} draft={draft} ready={}", open - draft);
        for p in snap.people.iter().filter(|p| p.total() > 0).take(8) {
            println!(
                "  {:<28} ready {:>2}  draft {:>2}  awaiting review {:>2}  oldest open {}",
                p.author,
                p.ready,
                p.draft,
                p.awaiting_review,
                p.oldest_open.map(format_duration).unwrap_or_else(|| "-".into())
            );
        }

        println!("\n-- spot checks --");
        for number in [2102, 2033, 2055, 2054] {
            let Some((pr, m)) = snap
                .prs
                .iter()
                .zip(&snap.metrics)
                .find(|(p, _)| p.number == number)
            else {
                println!("  #{number}: not in window");
                continue;
            };
            let prod = m
                .deploys
                .get("prod")
                .map(|d| match d {
                    DeployState::Deployed { tag, at } => format!("{tag} at {at}"),
                    other => format!("{other:?}"),
                })
                .unwrap_or_else(|| "-".into());
            println!(
                "  #{number} base={} author={} 1st review {} | ready->merge {} | prod: {}",
                pr.base_ref,
                pr.author,
                m.time_to_first_review.map(format_duration).unwrap_or_else(|| "-".into()),
                m.time_to_merge.map(format_duration).unwrap_or_else(|| "-".into()),
                prod
            );
            if let Some(m2d) = m.merge_to_deploy.get("prod") {
                println!("        merge->prod {}", format_duration(*m2d));
            }
            let names: Vec<&str> = pr.reviews.iter().map(|r| r.author.as_str()).collect();
            println!("        human reviewers: {names:?}");
        }

        println!("\n-- top reviewers --");
        for r in snap.reviewers.iter().take(6) {
            println!("  {:<28} {} reviews ({} approvals)", r.login, r.reviews_given, r.approvals_given);
        }
    }
    Ok(())
}
