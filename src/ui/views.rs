//! Tab rendering. One function per tab, plus the chrome and overlays around them.

use chrono::Utc;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap,
};

use crate::domain::metrics::format_duration;
use crate::domain::trunk::DeployState;
use crate::ui::app::{App, SyncStatus, Tab};
use crate::ui::widgets::{centered, duration_style, stat_card, truncate};

const HEADER: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(Color::DarkGray);

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // tab bar
        Constraint::Min(3),    // body
        Constraint::Length(1), // status bar
    ])
    .split(f.area());

    draw_tabs(f, chunks[0], app);

    if app.never_synced() {
        draw_empty(f, chunks[1], app);
    } else {
        match app.tab {
            Tab::Pipeline => pipeline(f, chunks[1], app),
            Tab::Review => review(f, chunks[1], app),
            Tab::People => people(f, chunks[1], app),
            Tab::Prs => prs(f, chunks[1], app),
        }
    }

    draw_status(f, chunks[2], app);

    if app.show_detail {
        draw_detail(f, f.area(), app);
    }
    if app.show_help {
        draw_help(f, f.area());
    }
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| Line::from(format!(" {} {} ", i + 1, t.title())))
        .collect();
    let selected = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
    f.render_widget(
        Tabs::new(titles)
            .select(selected)
            .style(DIM)
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).bold())
            .divider(""),
        area,
    );
}

fn draw_empty(f: &mut Frame, area: Rect, app: &App) {
    let repo = app
        .repo()
        .map(|r| r.slug())
        .unwrap_or_else(|| "(no repo configured)".into());
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No cached data yet.",
            Style::default().fg(Color::Yellow).bold(),
        )),
        Line::from(""),
        Line::from(format!("  Press r to fetch {repo}.")),
        Line::from(Span::styled(
            format!("  That reads the last {} days of PRs and deploy tags.", app.cfg.window_days),
            DIM,
        )),
    ];
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

// ------------------------------------------------------------------ pipeline

fn pipeline(f: &mut Frame, area: Rect, app: &App) {
    let Some(snap) = app.snapshot() else { return };
    let agg = &snap.aggregates;

    let rows = Layout::vertical([
        Constraint::Length(3), // review stages
        Constraint::Length(3), // deploy stages
        Constraint::Length(2), // coverage caveat
        Constraint::Min(3),    // stuck lists
    ])
    .split(area);

    let stage_cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(rows[0]);
    for (i, (title, s)) in [
        ("draft dwell", agg.draft_dwell),
        ("to 1st review", agg.time_to_first_review),
        ("review turnaround", agg.review_turnaround),
        ("ready to merge", agg.time_to_merge),
    ]
    .into_iter()
    .enumerate()
    {
        f.render_widget(stat_card(title, s), stage_cols[i]);
    }

    let deploy_cols = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(rows[1]);
    let usable_tags = snap.env_tags.get(&app.env).map(|t| t.len()).unwrap_or(0);
    f.render_widget(
        stat_card(
            &format!("merge to {} deploy ({usable_tags} tags)", app.env),
            agg.merge_to_deploy.get(&app.env).copied(),
        ),
        deploy_cols[0],
    );
    f.render_widget(
        stat_card(
            &format!("ready to {} deploy", app.env),
            agg.ready_to_deploy.get(&app.env).copied(),
        ),
        deploy_cols[1],
    );
    f.render_widget(
        stat_card(
            &format!("open to {} deploy", app.env),
            agg.open_to_deploy.get(&app.env).copied(),
        ),
        deploy_cols[2],
    );

    // The caveat travels with the number rather than living in a doc comment.
    let mut caveat = Vec::new();
    if let Some(warning) = app.coverage_warning() {
        caveat.push(Line::from(vec![
            Span::styled(" ! ", Style::default().fg(Color::Black).bg(Color::Yellow)),
            Span::styled(format!(" {warning}"), Style::default().fg(Color::Yellow)),
        ]));
    }
    if agg.stacked > 0 {
        caveat.push(Line::from(Span::styled(
            format!(
                "   {} PR(s) merged onto another branch rather than {} — excluded from deploy stats",
                agg.stacked, snap.repo.default_branch
            ),
            DIM,
        )));
    }
    f.render_widget(Paragraph::new(caveat), rows[2]);

    // Two stuck lists side by side: waiting on review, and merged but not deployed.
    let stuck = Layout::horizontal([Constraint::Ratio(1, 2); 2]).split(rows[3]);

    let awaiting = snap.awaiting_review();
    let now = Utc::now();
    let review_rows: Vec<Row> = awaiting
        .iter()
        .take(50)
        .map(|pr| {
            let waited = now - pr.ready_at.unwrap_or(pr.created_at);
            Row::new(vec![
                Cell::from(format!("#{}", pr.number)),
                Cell::from(truncate(&pr.title, 40)),
                Cell::from(truncate(&pr.author, 14)),
                Cell::from(Span::styled(format_duration(waited), duration_style(waited))),
            ])
        })
        .collect();
    f.render_widget(
        Table::new(
            review_rows,
            [
                Constraint::Length(7),
                Constraint::Min(20),
                Constraint::Length(15),
                Constraint::Length(9),
            ],
        )
        .header(Row::new(vec!["PR", "title", "author", "waiting"]).style(HEADER))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(DIM)
                .title(Span::styled(
                    format!(" awaiting first review ({}) ", awaiting.len()),
                    HEADER,
                )),
        ),
        stuck[0],
    );

    let undeployed = snap.awaiting_deploy(&app.env);
    let deploy_rows: Vec<Row> = undeployed
        .iter()
        .take(50)
        .map(|(pr, _)| {
            let since = pr.merged_at.map(|m| now - m);
            Row::new(vec![
                Cell::from(format!("#{}", pr.number)),
                Cell::from(truncate(&pr.title, 40)),
                Cell::from(truncate(&pr.author, 14)),
                Cell::from(match since {
                    Some(d) => Span::styled(format_duration(d), duration_style(d)),
                    None => Span::styled("-", DIM),
                }),
            ])
        })
        .collect();
    f.render_widget(
        Table::new(
            deploy_rows,
            [
                Constraint::Length(7),
                Constraint::Min(20),
                Constraint::Length(15),
                Constraint::Length(9),
            ],
        )
        .header(Row::new(vec!["PR", "title", "author", "merged"]).style(HEADER))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(DIM)
                .title(Span::styled(
                    format!(" merged, not in {} yet ({}) ", app.env, undeployed.len()),
                    HEADER,
                )),
        ),
        stuck[1],
    );
}

// -------------------------------------------------------------------- review

fn review(f: &mut Frame, area: Rect, app: &App) {
    let Some(snap) = app.snapshot() else { return };
    let split = Layout::horizontal([Constraint::Min(40), Constraint::Length(38)]).split(area);
    let now = Utc::now();

    let rows: Vec<Row> = app
        .rows()
        .iter()
        .map(|(pr, m)| {
            let waited = now - pr.ready_at.unwrap_or(pr.created_at);
            let first = m.time_to_first_review;
            Row::new(vec![
                Cell::from(format!("#{}", pr.number)),
                Cell::from(truncate(&pr.title, 44)),
                Cell::from(truncate(&pr.author, 14)),
                Cell::from(match first {
                    Some(d) => Span::styled(format_duration(d), duration_style(d)),
                    None => Span::styled("none yet", Style::default().fg(Color::Red)),
                }),
                Cell::from(Span::styled(format_duration(waited), duration_style(waited))),
                Cell::from(Span::styled(
                    pr.reviews.len().to_string(),
                    if pr.reviews.is_empty() { DIM } else { Style::default() },
                )),
            ])
        })
        .collect();

    render_table(
        f,
        split[0],
        app,
        rows,
        vec!["PR", "title", "author", "1st review", "open for", "revs"],
        vec![
            Constraint::Length(7),
            Constraint::Min(20),
            Constraint::Length(15),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(5),
        ],
        &format!(
            " {} open PRs in review — sorted by {} ",
            app.rows().len(),
            app.sort.label()
        ),
    );

    // Reviewer leaderboard: who absorbs the review load.
    let rev_rows: Vec<Row> = snap
        .reviewers
        .iter()
        .take(40)
        .map(|r| {
            Row::new(vec![
                Cell::from(truncate(&r.login, 22)),
                Cell::from(r.reviews_given.to_string()),
                Cell::from(r.approvals_given.to_string()),
            ])
        })
        .collect();
    f.render_widget(
        Table::new(
            rev_rows,
            [Constraint::Min(12), Constraint::Length(8), Constraint::Length(9)],
        )
        .header(Row::new(vec!["reviewer", "reviews", "approvals"]).style(HEADER))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(DIM)
                .title(Span::styled(" who reviews (bots excluded) ", HEADER)),
        ),
        split[1],
    );
}

// -------------------------------------------------------------------- people

fn people(f: &mut Frame, area: Rect, app: &App) {
    let Some(snap) = app.snapshot() else { return };

    let rows: Vec<Row> = snap
        .people
        .iter()
        .filter(|p| p.total() > 0)
        .map(|p| {
            Row::new(vec![
                Cell::from(truncate(&p.author, 24)),
                Cell::from(Span::styled(
                    p.ready.to_string(),
                    if p.ready >= 5 {
                        Style::default().fg(Color::Red).bold()
                    } else {
                        Style::default()
                    },
                )),
                Cell::from(Span::styled(p.draft.to_string(), DIM)),
                Cell::from(p.total().to_string()),
                Cell::from(Span::styled(
                    p.awaiting_review.to_string(),
                    if p.awaiting_review > 0 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        DIM
                    },
                )),
                Cell::from(match p.oldest_open {
                    Some(d) => Span::styled(format_duration(d), duration_style(d)),
                    None => Span::styled("-", DIM),
                }),
                Cell::from(match p.median_time_to_merge {
                    Some(d) => Span::styled(format_duration(d), duration_style(d)),
                    None => Span::styled("-", DIM),
                }),
                Cell::from(""),
            ])
        })
        .collect();

    let total: usize = snap.people.iter().map(|p| p.total()).sum();
    let drafts: usize = snap.people.iter().map(|p| p.draft).sum();

    let people_count = snap.people.iter().filter(|p| p.total() > 0).count();

    render_table(
        f,
        area,
        app,
        rows,
        vec![
            "person",
            "ready",
            "draft",
            "open",
            "no review",
            "oldest open",
            "median to merge",
            "",
        ],
        vec![
            Constraint::Length(24),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(16),
            // Absorbs leftover width so the name column stays readable rather than stretching.
            Constraint::Min(0),
        ],
        &format!(
            " threads of work — {people_count} people, {total} open ({} ready, {drafts} draft) ",
            total - drafts
        ),
    );
}

// ----------------------------------------------------------------------- prs

fn prs(f: &mut Frame, area: Rect, app: &App) {
    let rows: Vec<Row> = app
        .rows()
        .iter()
        .map(|(pr, m)| {
            let deploy = m.deploys.get(&app.env);
            Row::new(vec![
                Cell::from(format!("#{}", pr.number)),
                Cell::from(truncate(&pr.title, 46)),
                Cell::from(truncate(&pr.author, 14)),
                Cell::from(state_span(pr)),
                Cell::from(match m.time_to_first_review {
                    Some(d) => Span::styled(format_duration(d), duration_style(d)),
                    None => Span::styled("-", DIM),
                }),
                Cell::from(match m.ready_to_deploy.get(&app.env) {
                    Some(d) => Span::styled(format_duration(*d), duration_style(*d)),
                    None => deploy_span(deploy),
                }),
                Cell::from(match m.open_to_deploy.get(&app.env) {
                    Some(d) => Span::styled(format_duration(*d), duration_style(*d)),
                    None => deploy_span(deploy),
                }),
            ])
        })
        .collect();

    render_table(
        f,
        area,
        app,
        rows,
        vec!["PR", "title", "author", "state", "1st review", "ready→deploy", "open→deploy"],
        vec![
            Constraint::Length(7),
            Constraint::Min(20),
            Constraint::Length(15),
            Constraint::Length(8),
            Constraint::Length(11),
            Constraint::Length(13),
            Constraint::Length(12),
        ],
        &format!(
            " {} PRs — env {} — sorted by {} ",
            app.rows().len(),
            app.env,
            app.sort.label()
        ),
    );
}

fn state_span(pr: &crate::domain::pr::PullRequest) -> Span<'static> {
    use crate::domain::pr::PrState;
    match (pr.state, pr.is_draft) {
        (PrState::Open, true) => Span::styled("draft", DIM),
        (PrState::Open, false) => Span::styled("open", Style::default().fg(Color::Green)),
        (PrState::Merged, _) => Span::styled("merged", Style::default().fg(Color::Magenta)),
        (PrState::Closed, _) => Span::styled("closed", DIM),
    }
}

/// Render the non-duration deploy outcomes explicitly, so a blank cell never has to be interpreted.
fn deploy_span(state: Option<&DeployState>) -> Span<'static> {
    match state {
        Some(DeployState::NotYetDeployed) => {
            Span::styled("pending", Style::default().fg(Color::Yellow))
        }
        Some(DeployState::Stacked) => Span::styled("stacked", DIM),
        Some(DeployState::Unknown) => Span::styled("unknown", DIM),
        Some(DeployState::NotMerged) => Span::styled("-", DIM),
        _ => Span::styled("-", DIM),
    }
}

/// Shared table rendering with a highlighted selected row.
#[allow(clippy::too_many_arguments)]
fn render_table(
    f: &mut Frame,
    area: Rect,
    app: &App,
    rows: Vec<Row<'static>>,
    header: Vec<&'static str>,
    widths: Vec<Constraint>,
    title: &str,
) {
    let mut state = ratatui::widgets::TableState::default();
    state.select(Some(app.selected.min(rows.len().saturating_sub(1))));
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(HEADER))
        .row_highlight_style(Style::default().bg(Color::Rgb(40, 44, 60)).bold())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(DIM)
                .title(Span::styled(title.to_string(), HEADER)),
        );
    f.render_stateful_widget(table, area, &mut state);
}

// ------------------------------------------------------------------- chrome

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();

    match &app.status {
        SyncStatus::Running(stage) => spans.push(Span::styled(
            format!(" syncing: {stage} "),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )),
        SyncStatus::Failed(e) => spans.push(Span::styled(
            format!(" sync failed: {} ", truncate(e, 60)),
            Style::default().fg(Color::White).bg(Color::Red),
        )),
        SyncStatus::Idle => {
            let when = app
                .synced_at()
                .map(|t| {
                    let age = Utc::now() - t;
                    format!(" data {} old ", format_duration(age))
                })
                .unwrap_or_else(|| " no data ".to_string());
            spans.push(Span::styled(when, DIM));
        }
    }

    if let Some(repo) = app.repo() {
        let of = if app.repo_count() > 1 {
            format!(" ({}/{})", app.repo_index() + 1, app.repo_count())
        } else {
            String::new()
        };
        let prs = app
            .snapshot()
            .map(|s| format!("{} PRs", s.prs.len()))
            .unwrap_or_else(|| "not synced".into());
        spans.push(Span::styled(
            format!("│ {}{of} │ env {} │ {prs} ", repo.slug(), app.env),
            DIM,
        ));
    }

    if app.editing_filter {
        spans.push(Span::styled(
            format!("│ /{}", app.filter),
            Style::default().fg(Color::Cyan).bold(),
        ));
        spans.push(Span::styled("▏", Style::default().fg(Color::Cyan)));
    } else if !app.filter.is_empty() {
        spans.push(Span::styled(
            format!("│ filter: {} ", app.filter),
            Style::default().fg(Color::Cyan),
        ));
    }

    let repo_key = if app.repo_count() > 1 { "  R repo" } else { "" };
    spans.push(Span::styled(
        format!("│ r refresh{repo_key}  e env  s sort  / filter  ? help  q quit"),
        DIM,
    ));

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let rows = app.rows();
    let Some((pr, m)) = rows.get(app.selected) else {
        return;
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("#{} ", pr.number), HEADER),
            Span::raw(truncate(&pr.title, 60)),
        ]),
        Line::from(Span::styled(
            format!(
                "{} → {}  by {}  (+{} −{} over {} files)",
                pr.head_ref, pr.base_ref, pr.author, pr.additions, pr.deletions, pr.changed_files
            ),
            DIM,
        )),
        Line::from(""),
    ];

    let stage = |label: &str, d: Option<chrono::Duration>| {
        Line::from(vec![
            Span::styled(format!("  {label:<22}"), DIM),
            match d {
                Some(d) => Span::styled(format_duration(d), duration_style(d)),
                None => Span::styled("-", DIM),
            },
        ])
    };
    lines.push(Line::from(Span::styled("  timeline", HEADER)));
    lines.push(Line::from(vec![
        Span::styled(format!("  {:<22}", "review requested"), DIM),
        match pr.first_review_requested_at {
            Some(t) => Span::styled(
                t.format("%Y-%m-%d %H:%M").to_string(),
                Style::default().fg(Color::Gray),
            ),
            // Never asked is a different problem from asked-and-ignored.
            None => Span::styled("never requested", Style::default().fg(Color::Yellow)),
        },
    ]));
    lines.push(stage("draft dwell", m.draft_dwell));
    lines.push(stage("to first review", m.time_to_first_review));
    lines.push(stage("review turnaround", m.review_turnaround));
    lines.push(stage("ready to merge", m.time_to_merge));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  deploys ", HEADER),
        Span::styled("  from merge  from ready", DIM),
    ]));

    let since = |d: Option<&chrono::Duration>| {
        d.map(|d| format!("{:<12}", format_duration(*d)))
            .unwrap_or_else(|| format!("{:<12}", "-"))
    };
    for (env, state) in &m.deploys {
        let detail = match state {
            DeployState::Deployed { tag, at } => Line::from(vec![
                Span::styled(format!("  {env:<8}"), DIM),
                Span::styled(
                    format!(
                        "{}{}",
                        since(m.merge_to_deploy.get(env)),
                        since(m.ready_to_deploy.get(env))
                    ),
                    Style::default().fg(Color::Green),
                ),
                Span::styled(format!("{tag}  {}", at.format("%Y-%m-%d %H:%M")), DIM),
            ]),
            other => Line::from(vec![
                Span::styled(format!("  {env:<8}"), DIM),
                deploy_span(Some(other)),
            ]),
        };
        lines.push(detail);
    }

    if !pr.reviews.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  reviews (bots excluded)", HEADER)));
        for r in pr.reviews.iter().take(8) {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {:<20} {:<14} {}",
                    truncate(&r.author, 19),
                    r.state.to_lowercase(),
                    r.submitted_at.format("%Y-%m-%d %H:%M")
                ),
                DIM,
            )));
        }
    }

    if pr.truncated {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  note: GitHub truncated this PR's review or timeline history",
            Style::default().fg(Color::Yellow),
        )));
    }

    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = centered(area, 84, height);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" PR detail — Esc to close "),
            )
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let keys = [
        ("1-4 / Tab", "switch tab"),
        ("j k ↑ ↓", "move selection"),
        ("g G", "first / last row"),
        ("Enter", "PR detail"),
        ("r", "refresh from GitHub"),
        ("e", "cycle deploy environment"),
        ("R", "switch repository"),
        ("s", "cycle sort order"),
        ("/", "filter by title, author, number or label"),
        ("Esc", "close overlay or clear filter"),
        ("q", "quit"),
    ];
    let mut lines = vec![Line::from("")];
    for (k, d) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<12}"), Style::default().fg(Color::Cyan).bold()),
            Span::raw(d),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Data loads from a local cache at launch; r is the only thing that hits the network.",
        DIM,
    )));

    let rect = centered(area, 78, lines.len() as u16 + 2);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" keys — ? or Esc to close "),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::snapshot::fixtures;
    use crate::ui::app::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Sizes worth covering: a normal terminal, an awkward small one, and extremes.
    ///
    /// Layout arithmetic on tiny areas is the classic source of ratatui panics (subtracting borders
    /// from a 1-row rect, centring an overlay larger than the screen), so the narrow cases matter
    /// more than the comfortable one.
    const SIZES: [(u16, u16); 6] = [
        (200, 60),
        (120, 40),
        (80, 24),
        (40, 12),
        (20, 6),
        (8, 3),
    ];

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with_data() -> App {
        App::new(Config::default(), vec![Some(fixtures::snapshot())])
    }

    #[test]
    fn every_tab_renders_at_every_size() {
        for tab in Tab::ALL {
            for (w, h) in SIZES {
                let mut app = app_with_data();
                app.tab = tab;
                // Should not panic; that is the assertion.
                let out = render(&app, w, h);
                assert_eq!(out.lines().count(), h as usize);
            }
        }
    }

    #[test]
    fn overlays_render_at_every_size() {
        for (w, h) in SIZES {
            let mut app = app_with_data();
            app.show_help = true;
            render(&app, w, h);

            let mut app = app_with_data();
            app.tab = Tab::Prs;
            app.show_detail = true;
            render(&app, w, h);
        }
    }

    #[test]
    fn empty_state_tells_the_user_to_refresh() {
        let app = App::new(Config::default(), vec![None]);
        let out = render(&app, 80, 24);
        assert!(out.contains("No cached data"), "got:\n{out}");
        assert!(out.contains("Press r"), "got:\n{out}");
    }

    #[test]
    fn pipeline_shows_the_stuck_lists_and_the_coverage_caveat() {
        let mut app = app_with_data();
        app.tab = Tab::Pipeline;
        app.env = "dev".into();
        let out = render(&app, 200, 40);
        assert!(out.contains("awaiting first review"), "got:\n{out}");
        assert!(out.contains("not in dev yet"), "got:\n{out}");
        // The dev tag in the fixture is off-trunk, so the caveat must be visible.
        assert!(
            out.contains("branch deploys"),
            "partial coverage must be surfaced, got:\n{out}"
        );
    }

    #[test]
    fn people_tab_separates_ready_from_draft() {
        let mut app = app_with_data();
        app.tab = Tab::People;
        let out = render(&app, 140, 24);
        assert!(out.contains("ready"), "got:\n{out}");
        assert!(out.contains("draft"), "got:\n{out}");
        assert!(out.contains("threads of work"), "got:\n{out}");
    }

    #[test]
    fn prs_tab_labels_non_duration_deploy_outcomes() {
        let mut app = app_with_data();
        app.tab = Tab::Prs;
        app.sort = crate::ui::app::Sort::Newest;
        let out = render(&app, 160, 24);
        // The stacked PR must say so rather than showing a blank or a zero.
        assert!(out.contains("stacked"), "got:\n{out}");
    }

    #[test]
    fn filter_narrows_the_pr_list() {
        let mut app = app_with_data();
        app.tab = Tab::Prs;
        app.filter = "2102".into();
        assert_eq!(app.rows().len(), 1);
        let out = render(&app, 160, 24);
        assert!(out.contains("2102"), "got:\n{out}");
        assert!(!out.contains("#2099"), "got:\n{out}");
    }

    #[test]
    fn detail_view_distinguishes_never_requested_review() {
        let mut app = app_with_data();
        app.tab = Tab::Prs;
        app.sort = crate::ui::app::Sort::Newest;
        app.show_detail = true;
        let out = render(&app, 120, 40);
        assert!(out.contains("review requested"), "got:\n{out}");
    }
}
