use super::tree::{
    RepoInfo, RepoOperationStatus, TreeNode, TreeState, build_library_tree, build_tree,
    count_repos_in_trees, flatten_trees, toggle_node_at_path,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(PartialEq)]
pub enum AppMode {
    Normal,
    Search,
    CloneRepo,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Section {
    Workspace,
    Library,
}

impl Section {
    fn other(self) -> Self {
        match self {
            Section::Workspace => Section::Library,
            Section::Library => Section::Workspace,
        }
    }
}

/// A clone running in the background, shown as a temporary repo row in the
/// workspace tree until the real repo appears (or the failure expires)
struct PendingClone {
    display_name: String,
    status: RepoOperationStatus,
    failed_at: Option<Instant>,
}

pub struct App {
    workspace_repos_list: Vec<RepoInfo>,
    library_repos_list: Vec<RepoInfo>,
    /// What each panel renders: the repo lists grouped into a tree, with the
    /// transient status overlays and the search query already applied. Rebuilt
    /// from the lists whenever any of those change.
    workspace_tree: Vec<TreeNode>,
    library_tree: Vec<TreeNode>,
    pub workspace_state: TreeState,
    pub library_state: TreeState,
    pub search_query: String,
    pub active_section: Section,
    /// Whether the terminal is too narrow for the side-by-side layout; set
    /// each frame from the terminal width. Only the active section is
    /// rendered, and Tab may switch to an empty section to reveal it.
    pub single_panel: bool,
    pub matcher: SkimMatcherV2,
    pub workspace_path: String,
    pub loading_progress: Option<String>,
    pub watch_disabled: bool,
    pub mode: AppMode,
    /// Whether the keybindings overlay is shown (toggled with '?')
    pub help_visible: bool,
    /// Pre-rasterized logo for the help overlay; None renders text-only help
    pub help_image: Option<ratatui_image::protocol::StatefulProtocol>,
    pub clone_repo_input: String,
    pub clone_repo_suggestions: Vec<String>,
    pub clone_repo_state: TreeState,
    pub suggestions_loading: bool,
    pending_clones: Vec<PendingClone>,
    /// Sync status per repo display name (Fetching or SyncFailed), overlaid on
    /// the repo rows so it survives background data refreshes
    sync_statuses: std::collections::HashMap<String, RepoOperationStatus>,
    /// Full result of each repo's last sync, keyed by path, for the info
    /// panel's per-remote status rows
    pub sync_outcomes: std::collections::HashMap<PathBuf, crate::sync::SyncOutcome>,
    /// Repos with a fetch currently running, keyed by path
    pub fetching: std::collections::HashSet<PathBuf>,
    /// Info-panel details computed in the background, keyed by repo path
    pub details: std::collections::HashMap<PathBuf, super::details::RepoDetails>,
    /// Whether the current selection was made automatically (not by the user).
    /// Automatic selections may be replaced when repo data is reloaded; user
    /// selections are preserved.
    selection_is_auto: bool,
}

impl App {
    pub fn new(
        workspace_path: String,
        workspace_repos: Vec<RepoInfo>,
        library_repos: Vec<RepoInfo>,
    ) -> Self {
        let mut app = Self {
            workspace_repos_list: Vec::new(),
            library_repos_list: Vec::new(),
            workspace_tree: Vec::new(),
            library_tree: Vec::new(),
            workspace_state: TreeState::new(),
            library_state: TreeState::new(),
            search_query: String::new(),
            active_section: Section::Workspace,
            single_panel: false,
            matcher: SkimMatcherV2::default(),
            workspace_path,
            loading_progress: None,
            watch_disabled: false,
            mode: AppMode::Normal,
            help_visible: false,
            help_image: None,
            clone_repo_input: String::new(),
            clone_repo_suggestions: Vec::new(),
            clone_repo_state: TreeState::new(),
            suggestions_loading: false,
            pending_clones: Vec::new(),
            sync_statuses: std::collections::HashMap::new(),
            sync_outcomes: std::collections::HashMap::new(),
            fetching: std::collections::HashSet::new(),
            details: std::collections::HashMap::new(),
            selection_is_auto: true,
        };
        app.update_repos(workspace_repos, library_repos);
        app
    }

    pub fn filter_repos(&mut self) {
        self.rebuild_trees();
        self.select_first_available();
    }

    pub fn clear_search(&mut self) {
        self.search_query.clear();
        self.filter_repos();
    }

    /// Rebuild both section trees from the repo lists, applying the transient
    /// status overlays and the current search query
    fn rebuild_trees(&mut self) {
        // Repos are matched against the path shown in the tree; an empty
        // query matches everything
        let matches = |repo: &RepoInfo| {
            self.search_query.is_empty()
                || self
                    .matcher
                    .fuzzy_match(
                        repo.tree_path.as_deref().unwrap_or(&repo.display_name),
                        &self.search_query,
                    )
                    .is_some()
        };
        self.workspace_tree = build_tree(
            self.workspace_repos_with_overlays()
                .into_iter()
                .filter(|repo| matches(repo))
                .collect(),
        );
        self.library_tree = build_library_tree(
            self.library_repos_list
                .iter()
                .filter(|repo| matches(repo))
                .cloned()
                .collect(),
            &self.workspace_repos_list,
        );
    }

    /// The workspace repos with transient statuses applied: pending clones get
    /// their status overlaid on existing rows (cloning creates the directory
    /// right away) or a synthetic placeholder entry, and repos with an active
    /// sync get their sync status shown
    fn workspace_repos_with_overlays(&self) -> Vec<RepoInfo> {
        let mut repos = self.workspace_repos_list.clone();
        for repo in &mut repos {
            if let Some(status) = self.sync_statuses.get(&repo.display_name)
                && matches!(
                    repo.operation_status,
                    RepoOperationStatus::None | RepoOperationStatus::Scanning
                )
            {
                repo.operation_status = status.clone();
            }
        }
        for pending in &self.pending_clones {
            if let Some(repo) = repos
                .iter_mut()
                .find(|r| r.display_name == pending.display_name)
            {
                repo.operation_status = pending.status.clone();
            } else {
                repos.push(RepoInfo {
                    path: PathBuf::from(format!(
                        "{}/{}",
                        self.workspace_path, pending.display_name
                    )),
                    display_name: pending.display_name.clone(),
                    operation_status: pending.status.clone(),
                    ..Default::default()
                });
            }
        }
        repos
    }

    /// Rebuild both trees after the repo lists or their overlays changed. A
    /// selection made by the user follows its item; automatic selections are
    /// redone so the workspace is preferred once it has repos.
    fn rebuild_preserving_selection(&mut self) {
        let previous = if self.selection_is_auto {
            None
        } else {
            self.selected_position()
        };

        self.rebuild_trees();

        match previous {
            Some((section, index, path)) => self.restore_selection(section, index, &path),
            None => self.select_first_available(),
        }
    }

    /// Show a temporary "cloning..." row for the given repo pattern
    pub fn add_pending_clone(&mut self, display_name: String) {
        self.pending_clones
            .retain(|p| p.display_name != display_name);
        self.pending_clones.push(PendingClone {
            display_name,
            status: RepoOperationStatus::Cloning,
            failed_at: None,
        });
        self.rebuild_preserving_selection();
    }

    /// Resolve a pending clone: drop the row on success (the refresh brings in
    /// the real repo), or mark it failed so the error shows on the row
    pub fn finish_pending_clone(&mut self, display_name: &str, error: Option<String>) {
        match error {
            None => self
                .pending_clones
                .retain(|p| p.display_name != display_name),
            Some(err) => {
                if let Some(pending) = self
                    .pending_clones
                    .iter_mut()
                    .find(|p| p.display_name == display_name)
                {
                    pending.status = RepoOperationStatus::Failed(err);
                    pending.failed_at = Some(Instant::now());
                }
            }
        }
        self.rebuild_preserving_selection();
    }

    /// Remove failed clone rows older than `ttl` so they don't linger forever
    pub fn expire_failed_clones(&mut self, ttl: Duration) {
        let before = self.pending_clones.len();
        self.pending_clones
            .retain(|p| p.failed_at.is_none_or(|at| at.elapsed() < ttl));
        if self.pending_clones.len() != before {
            self.rebuild_preserving_selection();
        }
    }

    /// Select the first item in the first section that has any, preferring the
    /// workspace. Marks the selection as automatic.
    fn select_first_available(&mut self) {
        if !self.workspace_tree.is_empty() {
            self.select(Section::Workspace, 0);
        } else if !self.library_tree.is_empty() {
            self.select(Section::Library, 0);
        } else {
            self.workspace_state.select(None);
            self.library_state.select(None);
        }
        self.selection_is_auto = true;
    }

    pub fn get_flattened_workspace(&self) -> Vec<(&TreeNode, usize, Vec<usize>, String)> {
        flatten_trees(self.trees(Section::Workspace))
    }

    pub fn get_flattened_library(&self) -> Vec<(&TreeNode, usize, Vec<usize>, String)> {
        flatten_trees(self.trees(Section::Library))
    }

    pub fn count_workspace_repos(&self) -> usize {
        count_repos_in_trees(self.trees(Section::Workspace))
    }

    pub fn count_library_repos(&self) -> usize {
        count_repos_in_trees(self.trees(Section::Library))
    }

    /// The rendered trees of the given section
    fn trees(&self, section: Section) -> &[TreeNode] {
        match section {
            Section::Workspace => &self.workspace_tree,
            Section::Library => &self.library_tree,
        }
    }

    /// The selection state of the given section
    fn state(&self, section: Section) -> &TreeState {
        match section {
            Section::Workspace => &self.workspace_state,
            Section::Library => &self.library_state,
        }
    }

    /// Select the given index in the given section, clearing the other section
    fn select(&mut self, section: Section, index: usize) {
        let (target, other) = match section {
            Section::Workspace => (&mut self.workspace_state, &mut self.library_state),
            Section::Library => (&mut self.library_state, &mut self.workspace_state),
        };
        target.select(Some(index));
        other.select(None);
        self.active_section = section;
    }

    /// Number of visible (flattened) items in the given section
    fn section_len(&self, section: Section) -> usize {
        flatten_trees(self.trees(section)).len()
    }

    /// Switch to the other section if it has any items. In single-panel mode
    /// the other section is hidden, so Tab must reveal it even when empty.
    pub fn switch_section(&mut self) {
        let other = self.active_section.other();
        if self.section_len(other) > 0 {
            self.select(other, 0);
            self.selection_is_auto = false;
        } else if self.single_panel {
            self.active_section = other;
            self.workspace_state.select(None);
            self.library_state.select(None);
            self.selection_is_auto = false;
        }
    }

    pub fn next(&mut self) {
        self.move_selection(1);
    }

    pub fn previous(&mut self) {
        self.move_selection(-1);
    }

    /// Move the selection by one, crossing into the other section at the
    /// edges when both panels are visible
    fn move_selection(&mut self, delta: isize) {
        let section = self.active_section;
        let current_len = self.section_len(section);
        if current_len == 0 {
            return;
        }
        self.selection_is_auto = false;

        let Some(i) = self.state(section).selected() else {
            self.select(section, 0);
            return;
        };

        let at_edge = if delta > 0 {
            i + 1 >= current_len
        } else {
            i == 0
        };
        if !at_edge {
            self.select(section, i.saturating_add_signed(delta));
        } else if !self.single_panel && self.section_len(section.other()) > 0 {
            // Cross into the other section (top when moving down, bottom when moving up)
            let other_len = self.section_len(section.other());
            let index = if delta > 0 { 0 } else { other_len - 1 };
            self.select(section.other(), index);
        } else {
            // Wrap within the current section
            let index = if delta > 0 { 0 } else { current_len - 1 };
            self.select(section, index);
        }
    }

    /// The currently selected tree node in the active section
    pub fn selected_node(&self) -> Option<&TreeNode> {
        let index = self.state(self.active_section).selected()?;
        flatten_trees(self.trees(self.active_section))
            .get(index)
            .map(|(node, _, _, _)| *node)
    }

    pub fn toggle_expand(&mut self) {
        let index_path = self.state(self.active_section).selected().and_then(|i| {
            flatten_trees(self.trees(self.active_section))
                .get(i)
                .map(|(_, _, path, _)| path.clone())
        });

        if let Some(index_path) = index_path {
            let trees = match self.active_section {
                Section::Workspace => &mut self.workspace_tree,
                Section::Library => &mut self.library_tree,
            };
            toggle_node_at_path(trees, &index_path);
        }
    }

    /// Clone-dialog suggestions filtered by the current input
    pub fn filtered_suggestions(&self) -> Vec<&str> {
        let input = self.clone_repo_input.to_lowercase();
        self.clone_repo_suggestions
            .iter()
            .filter(|s| s.to_lowercase().contains(&input))
            .map(|s| s.as_str())
            .collect()
    }

    /// Mark a repo's row with the status of an operation running right now.
    /// Only the rendered trees are touched, so the mark lasts until the next
    /// rebuild — which is all `run_repo_operation` needs: it draws each step
    /// itself, and the rescan that follows reports the real outcome.
    pub fn update_repo_status(&mut self, display_name: &str, status: RepoOperationStatus) {
        update_repo_status_in_tree(&mut self.workspace_tree, display_name, status.clone());
        update_repo_status_in_tree(&mut self.library_tree, display_name, status);
    }

    /// Update the app with new repository data (for real-time loading).
    /// A selection made by the user is preserved across the update; automatic
    /// selections are redone so the workspace is preferred once it has repos.
    pub fn update_repos(&mut self, workspace_repos: Vec<RepoInfo>, library_repos: Vec<RepoInfo>) {
        // A rescan may have changed anything the info panel shows
        self.details.clear();

        self.workspace_repos_list = workspace_repos;
        self.library_repos_list = library_repos;
        self.rebuild_preserving_selection();
    }

    /// Overlay a sync status (Fetching or SyncFailed) on the given repo
    pub fn set_sync_status(&mut self, display_name: &str, status: RepoOperationStatus) {
        self.sync_statuses.insert(display_name.to_string(), status);
        self.rebuild_preserving_selection();
    }

    /// Remove the sync status overlay from the given repo
    pub fn clear_sync_status(&mut self, display_name: &str) {
        if self.sync_statuses.remove(display_name).is_some() {
            self.rebuild_preserving_selection();
        }
    }

    /// Apply a freshly computed git status to the given repo's row
    pub fn apply_scan_result(
        &mut self,
        display_name: &str,
        status: crate::RepoStatus,
        modification_time: Option<std::time::SystemTime>,
    ) {
        if let Some(repo) = self
            .workspace_repos_list
            .iter_mut()
            .find(|r| r.display_name == display_name)
        {
            repo.status = Some(status);
            if modification_time.is_some() {
                repo.modification_time = modification_time;
            }
            self.rebuild_preserving_selection();
        }
    }

    /// Path of the selected repo if background sync should fetch it: a
    /// workspace repo that isn't a submodule (submodules sync through their
    /// parent repo; library repos are never fetched)
    pub fn selected_syncable_repo_path(&self) -> Option<PathBuf> {
        if self.active_section != Section::Workspace {
            return None;
        }
        self.selected_node()?
            .repo_info
            .as_ref()
            .filter(|r| !r.is_submodule)
            .map(|r| r.path.clone())
    }

    /// Snapshot of the current repo lists, used to seed a background refresh
    /// so existing rows keep their data while being rescanned
    pub fn repo_snapshot(&self) -> (Vec<RepoInfo>, Vec<RepoInfo>) {
        (
            self.workspace_repos_list.clone(),
            self.library_repos_list.clone(),
        )
    }

    /// The section, index, and full path of the currently selected item
    fn selected_position(&self) -> Option<(Section, usize, String)> {
        let index = self.state(self.active_section).selected()?;
        let path = flatten_trees(self.trees(self.active_section))
            .get(index)
            .map(|(_, _, _, path)| path.clone())?;
        Some((self.active_section, index, path))
    }

    /// Re-select the item with the given full path, checking both sections so
    /// the selection follows a repo that was dropped or restored. Falls back to
    /// the nearest index in the previous section.
    fn restore_selection(&mut self, prev_section: Section, prev_index: usize, path: &str) {
        for section in [prev_section, prev_section.other()] {
            if let Some(index) = flatten_trees(self.trees(section))
                .iter()
                .position(|(_, _, _, p)| p == path)
            {
                self.select(section, index);
                return;
            }
        }

        let len = self.section_len(prev_section);
        if len > 0 {
            self.select(prev_section, prev_index.min(len - 1));
        } else {
            self.select_first_available();
        }
    }
}

fn update_repo_status_in_tree(
    nodes: &mut [TreeNode],
    display_name: &str,
    status: RepoOperationStatus,
) {
    for node in nodes {
        if let Some(ref mut repo) = node.repo_info
            && repo.display_name == display_name
        {
            repo.operation_status = status.clone();
        }
        update_repo_status_in_tree(&mut node.children, display_name, status.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo(display_name: &str) -> RepoInfo {
        RepoInfo {
            path: PathBuf::from(display_name),
            display_name: display_name.to_string(),
            status: Some(crate::RepoStatus::Clean),
            ..Default::default()
        }
    }

    fn selected_path(app: &App) -> Option<String> {
        app.selected_position().map(|(_, _, path)| path)
    }

    fn visible_workspace_paths(app: &App) -> Vec<String> {
        flatten_trees(&app.workspace_tree)
            .iter()
            .map(|(_, _, _, p)| p.clone())
            .collect()
    }

    fn workspace_repo_info(app: &App, name: &str) -> Option<RepoInfo> {
        flatten_trees(&app.workspace_tree)
            .iter()
            .find(|(_, _, _, p)| p == name)
            .and_then(|(node, _, _, _)| node.repo_info.clone())
    }

    #[test]
    fn sync_status_overlay_survives_updates() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            Vec::new(),
        );

        app.set_sync_status("github.com/foo/app", RepoOperationStatus::Fetching);
        let info = workspace_repo_info(&app, "github.com/foo/app").unwrap();
        assert_eq!(info.operation_status, RepoOperationStatus::Fetching);

        // A background rescan rebuilds the rows; the overlay must persist
        app.update_repos(vec![repo("github.com/foo/app")], Vec::new());
        let info = workspace_repo_info(&app, "github.com/foo/app").unwrap();
        assert_eq!(info.operation_status, RepoOperationStatus::Fetching);

        app.set_sync_status(
            "github.com/foo/app",
            RepoOperationStatus::SyncFailed("unreachable".to_string()),
        );
        let info = workspace_repo_info(&app, "github.com/foo/app").unwrap();
        assert_eq!(
            info.operation_status,
            RepoOperationStatus::SyncFailed("unreachable".to_string())
        );

        app.clear_sync_status("github.com/foo/app");
        let info = workspace_repo_info(&app, "github.com/foo/app").unwrap();
        assert_eq!(info.operation_status, RepoOperationStatus::None);
    }

    #[test]
    fn search_filters_both_sections_without_losing_overlays() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app"), repo("github.com/foo/tool")],
            vec![repo("github.com/bar/lib"), repo("github.com/bar/tool")],
        );
        app.set_sync_status("github.com/foo/tool", RepoOperationStatus::Fetching);
        assert_eq!(app.count_workspace_repos(), 2);
        assert_eq!(app.count_library_repos(), 2);

        // Both panels narrow to the matching repo, which keeps its overlay
        app.search_query = "tool".to_string();
        app.filter_repos();
        assert_eq!(app.count_workspace_repos(), 1);
        assert_eq!(app.count_library_repos(), 1);
        let info = workspace_repo_info(&app, "github.com/foo/tool").unwrap();
        assert_eq!(info.operation_status, RepoOperationStatus::Fetching);

        // A rescan landing while the filter is applied must not undo it
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/foo/tool")],
            vec![repo("github.com/bar/lib"), repo("github.com/bar/tool")],
        );
        assert_eq!(app.count_workspace_repos(), 1);
        assert_eq!(app.count_library_repos(), 1);

        // Clearing the search brings every repo back
        app.clear_search();
        assert_eq!(app.count_workspace_repos(), 2);
        assert_eq!(app.count_library_repos(), 2);
    }

    #[test]
    fn apply_scan_result_updates_repo_status() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            Vec::new(),
        );

        let time = std::time::SystemTime::now();
        app.apply_scan_result(
            "github.com/foo/app",
            crate::RepoStatus::Unpushed,
            Some(time),
        );

        let info = workspace_repo_info(&app, "github.com/foo/app").unwrap();
        assert_eq!(info.status, Some(crate::RepoStatus::Unpushed));
        assert_eq!(info.modification_time, Some(time));
    }

    #[test]
    fn pending_clone_appears_and_resolves() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            Vec::new(),
        );

        app.add_pending_clone("github.com/nosuch/thing".to_string());
        let names = visible_workspace_paths(&app);
        assert!(
            names.contains(&"github.com/nosuch/thing".to_string()),
            "placeholder missing: {:?}",
            names
        );

        // Cloning creates the directory immediately, so a rescan finds a real
        // repo mid-clone; the cloning status must stay on the row
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/nosuch/thing")],
            Vec::new(),
        );
        let status = flatten_trees(&app.workspace_tree)
            .iter()
            .find(|(_, _, _, p)| p == "github.com/nosuch/thing")
            .and_then(|(node, _, _, _)| node.repo_info.as_ref())
            .map(|r| r.operation_status.clone());
        assert!(matches!(status, Some(RepoOperationStatus::Cloning)));

        // Failure keeps the row so the error is visible
        app.finish_pending_clone("github.com/nosuch/thing", Some("boom".to_string()));
        assert!(visible_workspace_paths(&app).contains(&"github.com/nosuch/thing".to_string()));

        // Expiry removes the overlay (the row survives here because the repo
        // landed on disk above)
        app.expire_failed_clones(Duration::from_secs(0));
        let status = flatten_trees(&app.workspace_tree)
            .iter()
            .find(|(_, _, _, p)| p == "github.com/nosuch/thing")
            .and_then(|(node, _, _, _)| node.repo_info.as_ref())
            .map(|r| r.operation_status.clone());
        assert!(matches!(status, Some(RepoOperationStatus::None)));
    }

    #[test]
    fn auto_selection_prefers_workspace_once_it_loads() {
        // Library results arrive first (parallel loading is unordered)
        let mut app = App::new(
            "workspace".to_string(),
            Vec::new(),
            vec![repo("github.com/bar/lib")],
        );
        assert_eq!(app.active_section, Section::Library);

        // Workspace results arrive later; the automatic selection moves over
        app.update_repos(
            vec![repo("github.com/foo/app")],
            vec![repo("github.com/bar/lib")],
        );
        assert_eq!(app.active_section, Section::Workspace);

        // Further streaming updates keep it in the workspace
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/foo/other")],
            vec![repo("github.com/bar/lib")],
        );
        assert_eq!(app.active_section, Section::Workspace);
    }

    #[test]
    fn user_selection_survives_streaming_updates() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            vec![repo("github.com/bar/lib")],
        );

        // User moves into the library
        app.switch_section();
        assert_eq!(app.active_section, Section::Library);
        let path = selected_path(&app).unwrap();

        // More workspace repos stream in; the selection must not jump back
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/foo/other")],
            vec![repo("github.com/bar/lib")],
        );
        assert_eq!(app.active_section, Section::Library);
        assert_eq!(selected_path(&app).as_deref(), Some(path.as_str()));
    }

    #[test]
    fn selection_follows_item_when_siblings_shift() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app"), repo("github.com/foo/zeta")],
            Vec::new(),
        );

        // Move down to a specific repo
        app.next();
        app.next();
        let path = selected_path(&app).unwrap();

        // A new repo is inserted above it in the tree
        app.update_repos(
            vec![
                repo("github.com/aaa/first"),
                repo("github.com/foo/app"),
                repo("github.com/foo/zeta"),
            ],
            Vec::new(),
        );
        assert_eq!(selected_path(&app).as_deref(), Some(path.as_str()));
    }

    #[test]
    fn selection_follows_repo_across_sections() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            vec![repo("github.com/bar/lib")],
        );

        // User selects the library repo (a leaf, two levels deep)
        app.switch_section();
        app.next();
        app.next();
        let path = selected_path(&app).unwrap();
        assert_eq!(path, "github.com/bar/lib");

        // The repo is restored into the workspace
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/bar/lib")],
            Vec::new(),
        );
        assert_eq!(app.active_section, Section::Workspace);
        assert_eq!(selected_path(&app).as_deref(), Some(path.as_str()));
    }

    #[test]
    fn single_panel_tab_reaches_empty_section() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            Vec::new(),
        );
        app.single_panel = true;

        // Tab reveals the hidden (empty) library instead of being a no-op
        app.switch_section();
        assert_eq!(app.active_section, Section::Library);
        assert!(selected_path(&app).is_none());
        assert!(app.workspace_state.selected().is_none());

        // Tab again returns to the workspace with a selection
        app.switch_section();
        assert_eq!(app.active_section, Section::Workspace);
        assert_eq!(app.workspace_state.selected(), Some(0));
    }

    #[test]
    fn wide_mode_tab_still_refuses_empty_section() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            Vec::new(),
        );
        let before = selected_path(&app);

        app.switch_section();
        assert_eq!(app.active_section, Section::Workspace);
        assert_eq!(selected_path(&app), before);
    }

    #[test]
    fn single_panel_selection_wraps_within_section() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            vec![repo("github.com/bar/lib")],
        );
        app.single_panel = true;

        // Moving up from the top wraps to the bottom of the workspace instead
        // of crossing into the hidden library
        app.previous();
        assert_eq!(app.active_section, Section::Workspace);
        let last = app.section_len(Section::Workspace) - 1;
        assert_eq!(app.workspace_state.selected(), Some(last));

        // With both panels visible, the same move crosses into the library
        app.single_panel = false;
        app.next();
        assert_eq!(app.active_section, Section::Library);
        assert_eq!(app.library_state.selected(), Some(0));
    }
}
