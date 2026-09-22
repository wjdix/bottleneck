//! bottleneck — find where PRs get stuck.

mod config;
mod domain;
mod github;
mod snapshot;
mod store;
mod sync;
mod ui;

use anyhow::{Context, Result};
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

    /// Fetch from GitHub and update the cache, without starting the TUI.
    #[arg(long)]
    sync: bool,

    /// Render a tab to stdout from the cache and exit, without a terminal.
    ///
    /// Lets the real cached data be inspected in CI or a pipe, where no TTY exists.
    #[arg(long, value_name = "TAB")]
    render: Option<String>,

    /// Terminal size for --render, as WIDTHxHEIGHT.
    #[arg(long, default_value = "170x44")]
    size: String,

    /// Deploy environment for --render.
    #[arg(long)]
    env: Option<String>,

    /// Repository to act on, as owner/name. Defaults to the first configured.
    #[arg(long, value_name = "OWNER/NAME")]
    repo: Option<String>,

    /// Override the history window, in days.
    #[arg(long)]
    days: Option<i64>,

    /// Use an alternate config file instead of the one in the platform config directory.
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg_path = match args.config.clone() {
        Some(p) => p,
        None => config::config_path()?,
    };
    let mut cfg = Config::load(&cfg_path)?;
    if let Some(d) = args.days {
        cfg.window_days = d;
    }

    if args.dump_tags {
        return dump_tags(&cfg).await;
    }
    if args.dump_trunk {
        return dump_trunk(&cfg).await;
    }

    if cfg.repos.is_empty() {
        anyhow::bail!(
            "no repositories configured. Add one to {}",
            cfg_path.display()
        );
    }

    let db = config::db_path()?;
    let mut store = store::Store::open(&db)?;

    if args.sync {
        let gh = GitHub::new(GitHub::resolve_token().await?)?;
        for repo in &cfg.repos {
            eprintln!("{}", repo.slug());
            let snap = sync::refresh(&gh, &mut store, repo, &cfg, |stage| {
                eprintln!("  {stage}...");
            })
            .await?;
            eprintln!(
                "  {} PRs, {} trunk commits",
                snap.prs.len(),
                snap.trunk.len()
            );
        }
        eprintln!("cache: {}", db.display());
        return Ok(());
    }

    // Launch reads the cache only. Nothing here touches the network: a first run shows an empty
    // state telling the user to press r, rather than blocking on a fetch they did not ask for.
    let mut snapshots = Vec::with_capacity(cfg.repos.len());
    for repo in &cfg.repos {
        snapshots.push(sync::load_cached(&store, repo, &cfg)?);
    }

    if args.dump_deploys {
        for snap in snapshots.iter().flatten() {
            dump_deploys(snap);
        }
        return Ok(());
    }

    if let Some(tab) = args.render.as_deref() {
        return render_once(
            cfg,
            snapshots,
            tab,
            &args.size,
            args.env.as_deref(),
            args.repo.as_deref(),
        );
    }

    ui::run::run(cfg, store, snapshots).await
}

/// Draw one tab into an in-memory terminal and print it, so cached data can be checked without a
/// TTY.
fn render_once(
    cfg: Config,
    snapshots: Vec<Option<Snapshot>>,
    tab: &str,
    size: &str,
    env: Option<&str>,
    repo: Option<&str>,
) -> Result<()> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ui::app::{App, Tab};

    let (w, h) = size
        .split_once('x')
        .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
        .with_context(|| format!("--size must look like 170x44, got {size:?}"))?;

    let selected = Tab::ALL
        .iter()
        .find(|t| t.title().eq_ignore_ascii_case(tab))
        .copied()
        .with_context(|| {
            let names: Vec<&str> = Tab::ALL.iter().map(|t| t.title()).collect();
            format!("unknown tab {tab:?}; expected one of {}", names.join(", "))
        })?;

    let mut app = App::new(cfg, snapshots);
    app.tab = selected;
    if let Some(slug) = repo {
        // Cycle to the requested repo rather than indexing, so selection state stays consistent.
        let n = app.repo_count();
        let found = (0..n).any(|_| {
            if app.repo().is_some_and(|r| r.slug() == slug) {
                true
            } else {
                app.cycle_repo();
                false
            }
        });
        if !found && !app.repo().is_some_and(|r| r.slug() == slug) {
            anyhow::bail!("repo {slug:?} is not in the config");
        }
    }
    if let Some(env) = env {
        app.env = env.to_string();
    }

    let mut terminal = Terminal::new(TestBackend::new(w, h))?;
    terminal.draw(|f| ui::views::draw(f, &app))?;
    let buf = terminal.backend().buffer().clone();
    for y in 0..buf.area.height {
        let line: String = (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect();
        println!("{}", line.trim_end());
    }
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
        // Deliberately a full walk with no known-commit shortcut: this command exists to be
        // diffed against `git log --first-parent`, so it must not stop early.
        let scan = fetch::fetch_history(&gh, repo, since, &Default::default()).await?;
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

/// Print the analysed snapshot as plain text, so every headline number can be checked against
/// GitHub by hand. Reads the cache; run `--sync` first.
fn dump_deploys(snap: &Snapshot) {
    {
        let repo = &snap.repo;
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
            match snap.aggregates.ready_to_deploy.get(env) {
                Some(s) => print!(
                    "  |  ready->deploy median {:>9} p75 {:>9} (n={})",
                    format_duration(s.median),
                    format_duration(s.p75),
                    s.n
                ),
                None => print!("  |  ready->deploy no data"),
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
}
