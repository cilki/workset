use super::tree::{
    RepoInfo, RepoOperationStatus, TreeNode, build_library_tree, build_tree, count_repos_in_trees,
    flatten_trees, toggle_node_at_path,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::path::{Path, PathBuf};

#[derive(PartialEq)]
pub enum AppMode {
    Normal,
    Search,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Section {
    Workspace,
    Library,
}

impl Section {
    /// Both panels, in the order the selection prefers them
    const ALL: [Section; 2] = [Section::Workspace, Section::Library];

    fn other(self) -> Self {
        match self {
            Section::Workspace => Section::Library,
            Section::Library => Section::Workspace,
        }
    }

    /// Where this section's tree sits in [`App::trees`]
    fn index(self) -> usize {
        match self {
            Section::Workspace => 0,
            Section::Library => 1,
        }
    }
}

pub struct App {
    workspace_repos_list: Vec<RepoInfo>,
    library_repos_list: Vec<RepoInfo>,
    /// What each panel renders, indexed by [`Section`]: the repo lists
    /// grouped into a tree, with the transient status overlays and the search
    /// query already applied. Rebuilt from the lists whenever any of those
    /// change.
    trees: [Vec<TreeNode>; 2],
    /// The row the selection sits on, as an index into the active section's
    /// flattened tree.
    ///
    /// There is one selection, not one per panel: moving into a section is
    /// what makes it active, so the inactive panel never has a selected row.
    /// Two indices had to be kept in step to say that, with every selecting
    /// path responsible for clearing the other one.
    selected: Option<usize>,
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
    /// Sync status per repo path (Fetching or SyncFailed), overlaid on the
    /// repo rows so it survives background data refreshes. A repo is being
    /// fetched exactly when its entry here is Fetching.
    sync_statuses: std::collections::HashMap<PathBuf, RepoOperationStatus>,
    /// Full result of each repo's last sync, keyed by path, for the info
    /// panel's per-remote status rows
    pub sync_outcomes: std::collections::HashMap<PathBuf, crate::sync::SyncOutcome>,
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
            trees: [Vec::new(), Vec::new()],
            selected: None,
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
            sync_statuses: std::collections::HashMap::new(),
            sync_outcomes: std::collections::HashMap::new(),
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
        let workspace = build_tree(
            self.workspace_repos_with_overlays()
                .into_iter()
                .filter(|repo| matches(repo))
                .collect(),
        );
        let library = build_library_tree(
            self.library_repos_list
                .iter()
                .filter(|repo| matches(repo))
                .cloned()
                .collect(),
            &self.workspace_repos_list,
        );
        self.trees = [workspace, library];
    }

    /// The workspace repos with transient statuses applied: a repo with an
    /// active sync gets its sync status shown
    fn workspace_repos_with_overlays(&self) -> Vec<RepoInfo> {
        let mut repos = self.workspace_repos_list.clone();
        for repo in &mut repos {
            if let Some(status) = self.sync_statuses.get(&repo.path)
                && matches!(
                    repo.operation_status,
                    RepoOperationStatus::None | RepoOperationStatus::Scanning
                )
            {
                repo.operation_status = status.clone();
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

    /// Select the first item in the first section that has any, preferring the
    /// workspace. Marks the selection as automatic.
    fn select_first_available(&mut self) {
        self.selected = None;
        for section in Section::ALL {
            if !self.trees(section).is_empty() {
                self.select(section, 0);
                break;
            }
        }
        self.selection_is_auto = true;
    }

    /// The given section's rows, flattened for rendering
    pub fn flattened(&self, section: Section) -> Vec<(&TreeNode, usize, Vec<usize>, String)> {
        flatten_trees(self.trees(section))
    }

    /// How many repos the given section holds, after filtering
    pub fn count_repos(&self, section: Section) -> usize {
        count_repos_in_trees(self.trees(section))
    }

    /// The rendered trees of the given section
    fn trees(&self, section: Section) -> &[TreeNode] {
        &self.trees[section.index()]
    }

    /// The row selected in the given section, which is nothing at all unless
    /// the section is the active one
    pub fn selected_index(&self, section: Section) -> Option<usize> {
        (self.active_section == section)
            .then_some(self.selected)
            .flatten()
    }

    /// Make the given section active with the given row selected
    fn select(&mut self, section: Section, index: usize) {
        self.active_section = section;
        self.selected = Some(index);
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
            self.selected = None;
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

        let Some(i) = self.selected else {
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
        let index = self.selected?;
        flatten_trees(self.trees(self.active_section))
            .get(index)
            .map(|(node, _, _, _)| *node)
    }

    pub fn toggle_expand(&mut self) {
        let index_path = self.selected.and_then(|i| {
            flatten_trees(self.trees(self.active_section))
                .get(i)
                .map(|(_, _, path, _)| path.clone())
        });

        if let Some(index_path) = index_path {
            let section = self.active_section.index();
            toggle_node_at_path(&mut self.trees[section], &index_path);
        }
    }

    /// Mark a repo's row with the status of an operation running right now.
    /// Only the rendered trees are touched, so the mark lasts until the next
    /// rebuild — which is all `run_repo_operation` needs: it draws each step
    /// itself, and the rescan that follows reports the real outcome.
    pub fn update_repo_status(&mut self, display_name: &str, status: RepoOperationStatus) {
        for tree in &mut self.trees {
            update_repo_status_in_tree(tree, display_name, status.clone());
        }
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
    pub fn set_sync_status(&mut self, path: &Path, status: RepoOperationStatus) {
        self.sync_statuses.insert(path.to_path_buf(), status);
        self.rebuild_preserving_selection();
    }

    /// Remove the sync status overlay from the given repo
    pub fn clear_sync_status(&mut self, path: &Path) {
        if self.sync_statuses.remove(path).is_some() {
            self.rebuild_preserving_selection();
        }
    }

    /// Whether a fetch of the given repo is running right now
    pub fn is_fetching(&self, path: &Path) -> bool {
        self.sync_statuses.get(path) == Some(&RepoOperationStatus::Fetching)
    }

    /// Apply a freshly computed git status to the given repo's row
    pub fn apply_scan_result(
        &mut self,
        path: &Path,
        status: crate::RepoStatus,
        modification_time: Option<std::time::SystemTime>,
    ) {
        if let Some(repo) = self
            .workspace_repos_list
            .iter_mut()
            .find(|r| r.path == path)
        {
            repo.status = Some(status);
            if modification_time.is_some() {
                repo.modification_time = modification_time;
            }
            self.rebuild_preserving_selection();
        }
    }

    /// Path of the selected repo if background sync should fetch it: a
    /// workspace repo (library repos are never fetched)
    pub fn selected_syncable_repo_path(&self) -> Option<PathBuf> {
        if self.active_section != Section::Workspace {
            return None;
        }
        self.selected_node()?
            .repo_info
            .as_ref()
            .map(|r| r.path.clone())
    }

    /// Whether `path` is one of the repos in the working set, i.e. one of the
    /// rows the workspace panel holds.
    ///
    /// Anything acting on a path that came from outside the repo lists — the
    /// filesystem watcher, which reports every path under the workspace —
    /// has to ask, because "somewhere in the workspace directory" is not the
    /// same thing as "a repo workset manages". The working set is what
    /// [`crate::find_git_repositories`] reports, and it deliberately stops at
    /// the first repo on each branch of the walk: a git directory nested
    /// inside another repo's worktree is that repo's content, not a repo of
    /// ours, and a repo reached through a symlink lives somewhere the
    /// workspace never contained.
    ///
    /// Compared on canonical paths where both ends resolve, so a workspace
    /// reached through a symlink doesn't make every row fail to match a path
    /// spelled the other way.
    pub fn is_workspace_repo(&self, path: &Path) -> bool {
        let canonical = std::fs::canonicalize(path);
        self.workspace_repos_list.iter().any(|repo| {
            repo.path == path
                || match (&canonical, std::fs::canonicalize(&repo.path)) {
                    (Ok(path), Ok(row)) => row == *path,
                    _ => false,
                }
        })
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
        let index = self.selected?;
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

    fn workspace_repo_info(app: &App, name: &str) -> Option<RepoInfo> {
        flatten_trees(app.trees(Section::Workspace))
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

        app.set_sync_status(
            Path::new("github.com/foo/app"),
            RepoOperationStatus::Fetching,
        );
        let info = workspace_repo_info(&app, "github.com/foo/app").unwrap();
        assert_eq!(info.operation_status, RepoOperationStatus::Fetching);
        // The info panel reads "fetching" off the same overlay
        assert!(app.is_fetching(Path::new("github.com/foo/app")));

        // A background rescan rebuilds the rows; the overlay must persist
        app.update_repos(vec![repo("github.com/foo/app")], Vec::new());
        let info = workspace_repo_info(&app, "github.com/foo/app").unwrap();
        assert_eq!(info.operation_status, RepoOperationStatus::Fetching);

        app.set_sync_status(
            Path::new("github.com/foo/app"),
            RepoOperationStatus::SyncFailed("unreachable".to_string()),
        );
        let info = workspace_repo_info(&app, "github.com/foo/app").unwrap();
        assert_eq!(
            info.operation_status,
            RepoOperationStatus::SyncFailed("unreachable".to_string())
        );
        assert!(!app.is_fetching(Path::new("github.com/foo/app")));

        app.clear_sync_status(Path::new("github.com/foo/app"));
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
        app.set_sync_status(
            Path::new("github.com/foo/tool"),
            RepoOperationStatus::Fetching,
        );
        assert_eq!(app.count_repos(Section::Workspace), 2);
        assert_eq!(app.count_repos(Section::Library), 2);

        // Both panels narrow to the matching repo, which keeps its overlay
        app.search_query = "tool".to_string();
        app.filter_repos();
        assert_eq!(app.count_repos(Section::Workspace), 1);
        assert_eq!(app.count_repos(Section::Library), 1);
        let info = workspace_repo_info(&app, "github.com/foo/tool").unwrap();
        assert_eq!(info.operation_status, RepoOperationStatus::Fetching);

        // A rescan landing while the filter is applied must not undo it
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/foo/tool")],
            vec![repo("github.com/bar/lib"), repo("github.com/bar/tool")],
        );
        assert_eq!(app.count_repos(Section::Workspace), 1);
        assert_eq!(app.count_repos(Section::Library), 1);

        // Clearing the search brings every repo back
        app.clear_search();
        assert_eq!(app.count_repos(Section::Workspace), 2);
        assert_eq!(app.count_repos(Section::Library), 2);
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
            Path::new("github.com/foo/app"),
            crate::RepoStatus::Unpushed,
            Some(time),
        );

        let info = workspace_repo_info(&app, "github.com/foo/app").unwrap();
        assert_eq!(info.status, Some(crate::RepoStatus::Unpushed));
        assert_eq!(info.modification_time, Some(time));
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
        assert!(app.selected_index(Section::Workspace).is_none());

        // Tab again returns to the workspace with a selection
        app.switch_section();
        assert_eq!(app.active_section, Section::Workspace);
        assert_eq!(app.selected_index(Section::Workspace), Some(0));
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
        assert_eq!(app.selected_index(Section::Workspace), Some(last));

        // With both panels visible, the same move crosses into the library
        app.single_panel = false;
        app.next();
        assert_eq!(app.active_section, Section::Library);
        assert_eq!(app.selected_index(Section::Library), Some(0));
        // The selection crossed over, so the panel left behind highlights
        // nothing: only one row in the whole view is ever selected
        assert!(app.selected_index(Section::Workspace).is_none());
    }
}
