//! Application state and input handling.

use chrono::{DateTime, Utc};

use crate::config::Config;
use crate::domain::metrics::PrMetrics;
use crate::domain::pr::PullRequest;
use crate::snapshot::Snapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Pipeline,
    Review,
    People,
    Prs,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Pipeline, Tab::Review, Tab::People, Tab::Prs];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Pipeline => "Pipeline",
            Tab::Review => "Review",
            Tab::People => "People",
            Tab::Prs => "PRs",
        }
    }

    fn next(self) -> Self {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// How PR-ish tables are ordered. Each variant is "most interesting first".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    WaitingLongest,
    SlowestToDeploy,
    Newest,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::WaitingLongest => "waiting longest",
            Sort::SlowestToDeploy => "slowest to deploy",
            Sort::Newest => "newest",
        }
    }

    fn next(self) -> Self {
        match self {
            Sort::WaitingLongest => Sort::SlowestToDeploy,
            Sort::SlowestToDeploy => Sort::Newest,
            Sort::Newest => Sort::WaitingLongest,
        }
    }
}

/// What the refresh worker is doing.
#[derive(Debug, Clone)]
pub enum SyncStatus {
    Idle,
    Running(String),
    Failed(String),
}

pub struct App {
    pub cfg: Config,
    /// One slot per configured repo, in config order. `None` means never synced.
    ///
    /// Kept parallel to `cfg.repos` rather than keyed by slug so that adding a repo to the config is
    /// purely additive and needs no code change.
    snapshots: Vec<Option<Snapshot>>,
    repo_idx: usize,
    pub tab: Tab,
    pub sort: Sort,
    /// Index of the selected row in the active tab's table.
    pub selected: usize,
    /// Which env the deploy-focused views report on. Defaults to the first configured env.
    pub env: String,
    pub filter: String,
    pub editing_filter: bool,
    pub show_help: bool,
    pub show_detail: bool,
    pub status: SyncStatus,
    pub should_quit: bool,
}

impl App {
    /// Build with one snapshot slot per configured repo.
    pub fn new(cfg: Config, snapshots: Vec<Option<Snapshot>>) -> Self {
        // Keep the slots aligned with the config even if a caller passes too few.
        let mut snapshots = snapshots;
        snapshots.resize_with(cfg.repos.len().max(1), || None);

        let env = snapshots
            .iter()
            .flatten()
            .next()
            .and_then(|s| s.envs.first().cloned())
            .or_else(|| cfg.repos.first().and_then(|r| r.envs.first().cloned()))
            .unwrap_or_default();
        Self {
            cfg,
            snapshots,
            repo_idx: 0,
            tab: Tab::Pipeline,
            sort: Sort::WaitingLongest,
            selected: 0,
            env,
            filter: String::new(),
            editing_filter: false,
            show_help: false,
            show_detail: false,
            status: SyncStatus::Idle,
            should_quit: false,
        }
    }

    pub fn is_syncing(&self) -> bool {
        matches!(self.status, SyncStatus::Running(_))
    }

    /// The active repository's snapshot, if it has ever been synced.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshots.get(self.repo_idx).and_then(|s| s.as_ref())
    }

    /// True when the active repo has no cached data, so the UI can say so rather than show zeros.
    pub fn never_synced(&self) -> bool {
        self.snapshot().is_none()
    }

    /// The active repository.
    pub fn repo(&self) -> Option<&crate::config::RepoConfig> {
        self.cfg.repos.get(self.repo_idx)
    }

    pub fn repo_count(&self) -> usize {
        self.cfg.repos.len()
    }

    pub fn repo_index(&self) -> usize {
        self.repo_idx
    }

    /// Move to the next configured repo. A no-op when only one is configured.
    pub fn cycle_repo(&mut self) {
        if self.cfg.repos.len() < 2 {
            return;
        }
        self.repo_idx = (self.repo_idx + 1) % self.cfg.repos.len();
        self.selected = 0;
        // The new repo may not deploy to the env we were looking at.
        if let Some(snap) = self.snapshot()
            && !snap.envs.contains(&self.env)
            && let Some(first) = snap.envs.first().cloned()
        {
            self.env = first;
        }
    }

    /// Install a freshly synced snapshot into its repo's slot.
    ///
    /// Matched by slug rather than by the active index, because the user may have switched repos
    /// while the sync was in flight.
    pub fn set_snapshot(&mut self, snap: Snapshot) {
        if !snap.envs.contains(&self.env)
            && let Some(first) = snap.envs.first()
        {
            self.env = first.clone();
        }
        let slug = snap.repo.slug();
        let idx = self
            .cfg
            .repos
            .iter()
            .position(|r| r.slug() == slug)
            .unwrap_or(self.repo_idx);
        if idx < self.snapshots.len() {
            self.snapshots[idx] = Some(snap);
        }
        self.clamp_selection();
    }

    pub fn synced_at(&self) -> Option<DateTime<Utc>> {
        self.snapshot().map(|s| s.synced_at)
    }

    /// Rows for the active tab, already filtered and sorted.
    ///
    /// Returned as index pairs into the snapshot so callers can reach both the PR and its metrics
    /// without cloning either.
    pub fn rows(&self) -> Vec<(&PullRequest, &PrMetrics)> {
        let Some(snap) = self.snapshot() else {
            return Vec::new();
        };
        let needle = self.filter.to_lowercase();

        let mut rows: Vec<(&PullRequest, &PrMetrics)> = snap
            .prs
            .iter()
            .zip(&snap.metrics)
            .filter(|(pr, _)| {
                if needle.is_empty() {
                    return true;
                }
                pr.title.to_lowercase().contains(&needle)
                    || pr.author.to_lowercase().contains(&needle)
                    || pr.number.to_string().contains(&needle)
                    || pr.labels.iter().any(|l| l.to_lowercase().contains(&needle))
            })
            .filter(|(pr, _)| match self.tab {
                // The review view is about work that is actually waiting on a human.
                Tab::Review => pr.is_open() && !pr.is_draft,
                _ => true,
            })
            .collect();

        match self.sort {
            Sort::WaitingLongest => rows.sort_by_key(|(pr, m)| {
                // Open PRs sort by how long they have been waiting; closed ones by review latency.
                if pr.is_open() {
                    std::cmp::Reverse(
                        (Utc::now() - pr.ready_at.unwrap_or(pr.created_at)).num_seconds(),
                    )
                } else {
                    std::cmp::Reverse(
                        m.time_to_first_review.map(|d| d.num_seconds()).unwrap_or(0),
                    )
                }
            }),
            Sort::SlowestToDeploy => {
                let env = self.env.clone();
                rows.sort_by_key(|(_, m)| {
                    std::cmp::Reverse(
                        m.open_to_deploy
                            .get(&env)
                            .map(|d| d.num_seconds())
                            .unwrap_or(-1),
                    )
                });
            }
            Sort::Newest => rows.sort_by_key(|(pr, _)| std::cmp::Reverse(pr.number)),
        }
        rows
    }

    pub fn row_count(&self) -> usize {
        match self.tab {
            Tab::People => self.snapshot().map_or(0, |s| {
                s.people.iter().filter(|p| p.total() > 0).count()
            }),
            _ => self.rows().len(),
        }
    }

    fn clamp_selection(&mut self) {
        let n = self.row_count();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    /// Deploy environments available, for the env cycler.
    pub fn envs(&self) -> Vec<String> {
        self.snapshot()
            .map(|s| s.envs.clone())
            .or_else(|| self.repo().map(|r| r.envs.clone()))
            .unwrap_or_default()
    }

    pub fn cycle_env(&mut self) {
        let envs = self.envs();
        if envs.is_empty() {
            return;
        }
        let i = envs.iter().position(|e| *e == self.env).unwrap_or(0);
        self.env = envs[(i + 1) % envs.len()].clone();
    }

    /// Per-env coverage caveat for the selected env, when it is worth showing.
    pub fn coverage_warning(&self) -> Option<String> {
        let snap = self.snapshot()?;
        let cov = snap.coverage.get(&self.env)?;
        cov.is_partial().then(|| {
            format!(
                "only {:.0}% of {} tags are cut from {} — the rest are branch deploys, so {} latency is overstated",
                cov.fraction_on_trunk() * 100.0,
                self.env,
                snap.repo.default_branch,
                self.env,
            )
        })
    }
}

/// What a key press asked for. Returned so the event loop, which owns the network, can act on
/// requests that `App` cannot fulfil by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Refresh,
    Quit,
}

/// Keys handled uniformly across tabs.
pub fn handle_key(app: &mut App, key: crossterm::event::KeyEvent) -> Action {
    use crossterm::event::{KeyCode, KeyModifiers};

    // Filter editing swallows almost everything, so it is handled first.
    if app.editing_filter {
        match key.code {
            KeyCode::Esc => {
                app.editing_filter = false;
                app.filter.clear();
            }
            KeyCode::Enter => app.editing_filter = false,
            KeyCode::Backspace => {
                app.filter.pop();
            }
            KeyCode::Char(c) => app.filter.push(c),
            _ => {}
        }
        app.clamp_selection();
        return Action::None;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return Action::Quit;
    }

    match key.code {
        KeyCode::Char('q') => {
            app.should_quit = true;
            return Action::Quit;
        }
        KeyCode::Char('?') => app.show_help = !app.show_help,
        KeyCode::Esc => {
            if app.show_help {
                app.show_help = false;
            } else if app.show_detail {
                app.show_detail = false;
            } else if !app.filter.is_empty() {
                app.filter.clear();
                app.clamp_selection();
            }
        }
        KeyCode::Char('r') => return Action::Refresh,
        KeyCode::Char('/') => {
            app.editing_filter = true;
            app.filter.clear();
        }
        KeyCode::Char('s') => app.sort = app.sort.next(),
        KeyCode::Char('e') => app.cycle_env(),
        KeyCode::Char('R') => app.cycle_repo(),
        KeyCode::Tab | KeyCode::Right => {
            app.tab = app.tab.next();
            app.selected = 0;
        }
        KeyCode::BackTab | KeyCode::Left => {
            app.tab = app.tab.prev();
            app.selected = 0;
        }
        KeyCode::Char(c @ '1'..='4') => {
            let i = c as usize - '1' as usize;
            app.tab = Tab::ALL[i];
            app.selected = 0;
        }
        KeyCode::Enter => app.show_detail = !app.show_detail,
        KeyCode::Down | KeyCode::Char('j') => {
            let n = app.row_count();
            if n > 0 {
                app.selected = (app.selected + 1).min(n - 1);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => app.selected = app.selected.saturating_sub(1),
        KeyCode::Home | KeyCode::Char('g') => app.selected = 0,
        KeyCode::End | KeyCode::Char('G') => {
            app.selected = app.row_count().saturating_sub(1);
        }
        KeyCode::PageDown => {
            let n = app.row_count();
            if n > 0 {
                app.selected = (app.selected + 10).min(n - 1);
            }
        }
        KeyCode::PageUp => app.selected = app.selected.saturating_sub(10),
        _ => {}
    }
    Action::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app() -> App {
        App::new(Config::example(), vec![None])
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn tab_cycles_forward_and_back() {
        let mut a = app();
        assert_eq!(a.tab, Tab::Pipeline);
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.tab, Tab::Review);
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.tab, Tab::Pipeline);
        // Wraps around rather than stopping.
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.tab, Tab::Prs);
    }

    #[test]
    fn number_keys_jump_to_tabs() {
        let mut a = app();
        press(&mut a, KeyCode::Char('3'));
        assert_eq!(a.tab, Tab::People);
        press(&mut a, KeyCode::Char('1'));
        assert_eq!(a.tab, Tab::Pipeline);
    }

    #[test]
    fn r_requests_a_refresh_without_mutating_state() {
        let mut a = app();
        assert_eq!(press(&mut a, KeyCode::Char('r')), Action::Refresh);
        assert!(!a.should_quit);
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        let mut a = app();
        assert_eq!(press(&mut a, KeyCode::Char('q')), Action::Quit);
        assert!(a.should_quit);

        let mut b = app();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(handle_key(&mut b, ctrl_c), Action::Quit);
        assert!(b.should_quit);
    }

    #[test]
    fn filter_editing_captures_text_and_does_not_quit() {
        let mut a = app();
        press(&mut a, KeyCode::Char('/'));
        assert!(a.editing_filter);
        // 'q' must be typed into the filter, not quit the app.
        for c in "qa fix".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        assert_eq!(a.filter, "qa fix");
        assert!(!a.should_quit);
        press(&mut a, KeyCode::Backspace);
        assert_eq!(a.filter, "qa fi");
        press(&mut a, KeyCode::Enter);
        assert!(!a.editing_filter);
    }

    #[test]
    fn esc_while_editing_clears_the_filter() {
        let mut a = app();
        press(&mut a, KeyCode::Char('/'));
        press(&mut a, KeyCode::Char('x'));
        press(&mut a, KeyCode::Esc);
        assert!(!a.editing_filter);
        assert!(a.filter.is_empty());
    }

    #[test]
    fn selection_cannot_go_negative_or_past_the_end_with_no_rows() {
        let mut a = app();
        press(&mut a, KeyCode::Up);
        assert_eq!(a.selected, 0);
        press(&mut a, KeyCode::Down);
        assert_eq!(a.selected, 0, "no rows means selection stays put");
        press(&mut a, KeyCode::End);
        assert_eq!(a.selected, 0);
    }

    #[test]
    fn sort_cycles_through_every_mode() {
        let mut a = app();
        assert_eq!(a.sort, Sort::WaitingLongest);
        press(&mut a, KeyCode::Char('s'));
        assert_eq!(a.sort, Sort::SlowestToDeploy);
        press(&mut a, KeyCode::Char('s'));
        assert_eq!(a.sort, Sort::Newest);
        press(&mut a, KeyCode::Char('s'));
        assert_eq!(a.sort, Sort::WaitingLongest);
    }

    #[test]
    fn env_cycles_through_configured_envs() {
        let mut a = app();
        let envs = a.envs();
        assert!(envs.len() > 1);
        assert_eq!(a.env, envs[0]);
        press(&mut a, KeyCode::Char('e'));
        assert_eq!(a.env, envs[1]);
    }

    #[test]
    fn help_toggles_and_esc_closes_it() {
        let mut a = app();
        press(&mut a, KeyCode::Char('?'));
        assert!(a.show_help);
        press(&mut a, KeyCode::Esc);
        assert!(!a.show_help);
    }

    fn two_repo_cfg() -> Config {
        let mut cfg = Config::example();
        let mut other = cfg.repos[0].clone();
        other.name = "other".into();
        cfg.repos.push(other);
        cfg
    }

    #[test]
    fn capital_r_cycles_repositories() {
        let mut a = App::new(two_repo_cfg(), vec![None, None]);
        assert_eq!(a.repo().unwrap().slug(), "pagos-ai/pagos");
        press(&mut a, KeyCode::Char('R'));
        assert_eq!(a.repo().unwrap().slug(), "pagos-ai/other");
        press(&mut a, KeyCode::Char('R'));
        assert_eq!(a.repo().unwrap().slug(), "pagos-ai/pagos", "wraps around");
    }

    #[test]
    fn cycling_repos_is_a_no_op_with_one_repo() {
        let mut a = app();
        assert_eq!(a.repo_count(), 1);
        press(&mut a, KeyCode::Char('R'));
        assert_eq!(a.repo_index(), 0);
    }

    /// Each repo keeps its own cached data, so switching must not show the other repo's numbers.
    #[test]
    fn each_repo_has_its_own_snapshot_slot() {
        let snap = crate::snapshot::fixtures::snapshot();
        let mut a = App::new(two_repo_cfg(), vec![Some(snap), None]);
        assert!(!a.never_synced());
        press(&mut a, KeyCode::Char('R'));
        assert!(a.never_synced(), "the second repo was never synced");
        press(&mut a, KeyCode::Char('R'));
        assert!(!a.never_synced());
    }

    /// A sync finishing after the user switched repos must land in the right slot.
    #[test]
    fn a_finished_sync_lands_in_its_own_repos_slot() {
        let mut a = App::new(two_repo_cfg(), vec![None, None]);
        // User switches to repo 2 while a sync for repo 1 is still running.
        press(&mut a, KeyCode::Char('R'));
        assert_eq!(a.repo_index(), 1);

        a.set_snapshot(crate::snapshot::fixtures::snapshot()); // this is pagos-ai/pagos
        assert!(a.never_synced(), "repo 2 must still be empty");
        press(&mut a, KeyCode::Char('R'));
        assert!(!a.never_synced(), "repo 1 received the snapshot");
    }

    /// The config having more repos than the caller passed slots for must not panic.
    #[test]
    fn slots_are_padded_to_match_the_config() {
        let a = App::new(two_repo_cfg(), vec![]);
        assert_eq!(a.repo_count(), 2);
        assert!(a.snapshot().is_none());
    }

    #[test]
    fn no_snapshot_is_reported_rather_than_shown_as_zeros() {
        let a = app();
        assert!(a.never_synced());
        assert_eq!(a.row_count(), 0);
        assert!(a.rows().is_empty());
    }
}
