use std::path::{Path, PathBuf};

pub struct TreeState {
    /// Currently selected item index (in flattened view)
    selected: Option<usize>,
}

impl TreeState {
    pub fn new() -> Self {
        Self { selected: None }
    }

    pub fn select(&mut self, index: Option<usize>) {
        self.selected = index;
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum RepoOperationStatus {
    #[default]
    None,
    /// A background git status scan is running for this repo
    Scanning,
    /// A background sync is fetching the repo's remotes
    Fetching,
    /// Commits are being pushed to the repo's mirror remotes
    Syncing,
    /// The last mirror check found refs waiting to be pushed ('s' pushes them)
    PushPending(usize),
    /// The last mirror attempt failed or hit a conflict
    SyncFailed(String),
    Cloning,
    Dropping,
    Restoring,
    Success,
    Failed(String),
}

#[derive(Clone, Default)]
pub struct RepoInfo {
    pub path: PathBuf,
    pub display_name: String,
    /// Where this repo sits in the tree: host/path derived from its origin
    /// (or first) remote URL. None falls back to `display_name`, i.e. the
    /// on-disk layout. Never used to identify the repo — only to group it.
    pub tree_path: Option<String>,
    /// Git status, or None while it hasn't been scanned yet
    pub status: Option<crate::RepoStatus>,
    /// Modification time (for sorting and display)
    pub modification_time: Option<std::time::SystemTime>,
    /// Size on disk in bytes
    pub size_bytes: Option<u64>,
    /// Current operation status
    pub operation_status: RepoOperationStatus,
    /// Whether this repo is a submodule
    pub is_submodule: bool,
    /// Whether this submodule is initialized (checked out)
    pub submodule_initialized: bool,
    /// Path to parent repository (for submodules)
    pub parent_repo_path: Option<PathBuf>,
}

#[derive(Clone)]
pub struct TreeNode {
    /// The display name for this node (just the name, not full path)
    pub name: String,
    /// Full path if this is a repo, None if just a directory
    pub repo_info: Option<RepoInfo>,
    /// Children of this node
    pub children: Vec<TreeNode>,
    /// Whether this node is expanded
    pub expanded: bool,
}

impl TreeNode {
    pub fn new_directory(name: String) -> Self {
        Self {
            name,
            repo_info: None,
            children: Vec::new(),
            expanded: true,
        }
    }

    /// Flatten the tree into a list of (node, depth, index_path, full_path) tuples
    pub fn flatten(
        &self,
        depth: usize,
        index_path: Vec<usize>,
        parent_path: &str,
    ) -> Vec<(&TreeNode, usize, Vec<usize>, String)> {
        // Build the full path for this node
        let full_path = if let Some(ref repo) = self.repo_info {
            repo.display_name.clone()
        } else if parent_path.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", parent_path, self.name)
        };

        let mut result = vec![(self, depth, index_path.clone(), full_path.clone())];

        if self.expanded {
            for (i, child) in self.children.iter().enumerate() {
                let mut child_index_path = index_path.clone();
                child_index_path.push(i);
                result.extend(child.flatten(depth + 1, child_index_path, &full_path));
            }
        }

        result
    }

    /// Toggle expanded state
    pub fn toggle_expand(&mut self) {
        if !self.children.is_empty() {
            self.expanded = !self.expanded;
        }
    }

    /// Collect all repo paths in this subtree
    pub fn collect_repo_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        if let Some(ref repo) = self.repo_info {
            paths.push(repo.display_name.clone());
        }
        for child in &self.children {
            paths.extend(child.collect_repo_paths());
        }
        paths
    }

    /// Collect the filesystem paths of syncable repos in this subtree
    /// (submodules are synced through their parent repo)
    pub fn collect_syncable_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(ref repo) = self.repo_info
            && !repo.is_submodule
        {
            paths.push(repo.path.clone());
        }
        for child in &self.children {
            paths.extend(child.collect_syncable_paths());
        }
        paths
    }

    /// Count repos in this subtree
    pub fn count_repos(&self) -> usize {
        let mut count = if self.repo_info.is_some() { 1 } else { 0 };
        for child in &self.children {
            count += child.count_repos();
        }
        count
    }
}

/// Build a tree structure from a flat list of repos
pub fn build_tree(mut repos: Vec<RepoInfo>) -> Vec<TreeNode> {
    // Separate regular repos from submodules
    let (submodules, regular_repos): (Vec<_>, Vec<_>) =
        repos.drain(..).partition(|r| r.is_submodule);

    // Sort regular repos by modification time (most recent first)
    let mut sorted_repos = regular_repos;
    sorted_repos.sort_by(|a, b| {
        match (a.modification_time, b.modification_time) {
            (Some(a_time), Some(b_time)) => b_time.cmp(&a_time), // Most recent first
            (Some(_), None) => std::cmp::Ordering::Less,         // Items with time come first
            (None, Some(_)) => std::cmp::Ordering::Greater,      // Items without time come last
            (None, None) => a.display_name.cmp(&b.display_name), // Fallback to name
        }
    });

    let mut root_nodes: Vec<TreeNode> = Vec::new();

    // Build tree from regular repos, grouped by remote URL when known. Two
    // clones of the same URL collide on the same node; the later one falls
    // back to its on-disk placement so both stay visible.
    for repo in sorted_repos {
        let grouping = repo
            .tree_path
            .clone()
            .unwrap_or_else(|| repo.display_name.clone());
        if !insert_repo_at(&mut root_nodes, &grouping, &repo) && repo.tree_path.is_some() {
            let fallback = repo.display_name.clone();
            insert_repo_at(&mut root_nodes, &fallback, &repo);
        }
    }

    // Now insert submodules as children of their parent repos
    for submodule in submodules {
        insert_submodule_into_tree(&mut root_nodes, submodule);
    }

    root_nodes
}

/// Insert a repo at the slash-separated path, creating directory nodes along
/// the way. Returns false when another repo already occupies the target node.
fn insert_repo_at(root_nodes: &mut Vec<TreeNode>, path: &str, repo: &RepoInfo) -> bool {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return true;
    }

    let mut current_level = root_nodes;
    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        let node_idx = current_level.iter().position(|n| n.name == *part);

        if let Some(idx) = node_idx {
            if is_last {
                if current_level[idx].repo_info.is_some() {
                    return false;
                }
                current_level[idx].repo_info = Some(repo.clone());
            }
            current_level = &mut current_level[idx].children;
        } else {
            let new_node = if is_last {
                TreeNode {
                    name: (*part).to_string(),
                    repo_info: Some(repo.clone()),
                    children: Vec::new(),
                    expanded: false,
                }
            } else {
                TreeNode::new_directory((*part).to_string())
            };
            current_level.push(new_node);
            let new_idx = current_level.len() - 1;
            current_level = &mut current_level[new_idx].children;
        }
    }
    true
}

/// Helper function to insert a submodule into the tree as a child of its parent repo
fn insert_submodule_into_tree(root_nodes: &mut [TreeNode], submodule: RepoInfo) {
    let parent_path = match submodule.parent_repo_path.clone() {
        Some(path) => path,
        None => return, // Shouldn't happen, but skip if no parent
    };

    // Find the parent repo node and add the submodule as a child
    if let Some(parent_node) = find_repo_node_by_path(root_nodes, &parent_path) {
        let name = submodule
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&submodule.display_name)
            .to_string();

        parent_node.children.push(TreeNode {
            name,
            repo_info: Some(submodule),
            children: Vec::new(),
            expanded: false, // Submodules start collapsed
        });
    }
}

/// Recursively find a tree node by its repository path
fn find_repo_node_by_path<'a>(nodes: &'a mut [TreeNode], path: &Path) -> Option<&'a mut TreeNode> {
    for node in nodes {
        // Check if this node matches
        if let Some(ref repo_info) = node.repo_info
            && repo_info.path == path
        {
            return Some(node);
        }

        // Recursively check children
        if let Some(found) = find_repo_node_by_path(&mut node.children, path) {
            return Some(found);
        }
    }
    None
}

/// Build library tree, excluding repos that exist in workspace
pub fn build_library_tree(
    library_repos: Vec<RepoInfo>,
    workspace_repos: &[RepoInfo],
) -> Vec<TreeNode> {
    let workspace_paths: std::collections::HashSet<_> = workspace_repos
        .iter()
        .map(|r| r.display_name.as_str())
        .collect();

    let filtered_repos: Vec<RepoInfo> = library_repos
        .into_iter()
        .filter(|repo| !workspace_paths.contains(repo.display_name.as_str()))
        .collect();

    build_tree(filtered_repos)
}

/// Flatten a forest of trees into a list
pub fn flatten_trees(trees: &[TreeNode]) -> Vec<(&TreeNode, usize, Vec<usize>, String)> {
    let mut result = Vec::new();
    for (i, tree) in trees.iter().enumerate() {
        result.extend(tree.flatten(0, vec![i], ""));
    }
    result
}

/// Count repos in a forest of trees
pub fn count_repos_in_trees(trees: &[TreeNode]) -> usize {
    trees.iter().map(|t| t.count_repos()).sum()
}

pub fn toggle_node_at_path(mut nodes: &mut [TreeNode], path: &[usize]) {
    let Some((&last, parents)) = path.split_last() else {
        return;
    };

    for &idx in parents {
        match nodes.get_mut(idx) {
            Some(node) => nodes = &mut node.children,
            None => return,
        }
    }

    if let Some(node) = nodes.get_mut(last) {
        node.toggle_expand();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(display_name: &str, tree_path: Option<&str>) -> RepoInfo {
        RepoInfo {
            path: PathBuf::from(format!("/ws/{display_name}")),
            display_name: display_name.to_string(),
            tree_path: tree_path.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    fn node_at<'a>(nodes: &'a [TreeNode], path: &[&str]) -> Option<&'a TreeNode> {
        let (first, rest) = path.split_first()?;
        let node = nodes.iter().find(|n| n.name == *first)?;
        if rest.is_empty() {
            Some(node)
        } else {
            node_at(&node.children, rest)
        }
    }

    #[test]
    fn collect_syncable_paths_skips_submodules() {
        let tree = build_tree(vec![
            repo("work/foo", None),
            repo("work/bar", None),
            RepoInfo {
                is_submodule: true,
                parent_repo_path: Some(PathBuf::from("/ws/work/foo")),
                ..repo("work/foo/sub", None)
            },
        ]);
        let mut paths: Vec<PathBuf> = tree
            .iter()
            .flat_map(|node| node.collect_syncable_paths())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![PathBuf::from("/ws/work/bar"), PathBuf::from("/ws/work/foo")]
        );
    }

    #[test]
    fn repos_grouped_by_remote_tree_path() {
        let tree = build_tree(vec![repo("work/foo", Some("github.com/fossable/foo"))]);
        let node = node_at(&tree, &["github.com", "fossable", "foo"]).unwrap();
        assert_eq!(
            node.repo_info.as_ref().unwrap().path,
            PathBuf::from("/ws/work/foo")
        );
        assert!(node_at(&tree, &["work"]).is_none());
    }

    #[test]
    fn repos_without_remote_fall_back_to_disk_layout() {
        let tree = build_tree(vec![repo("work/foo", None)]);
        assert!(node_at(&tree, &["work", "foo"]).unwrap().repo_info.is_some());
    }

    #[test]
    fn clones_of_the_same_url_both_stay_visible() {
        let tree = build_tree(vec![
            repo("a/one", Some("github.com/x/y")),
            repo("b/two", Some("github.com/x/y")),
        ]);
        assert_eq!(count_repos_in_trees(&tree), 2);
        let primary = node_at(&tree, &["github.com", "x", "y"]).unwrap();
        assert_eq!(
            primary.repo_info.as_ref().unwrap().path,
            PathBuf::from("/ws/a/one")
        );
        let fallback = node_at(&tree, &["b", "two"]).unwrap();
        assert_eq!(
            fallback.repo_info.as_ref().unwrap().path,
            PathBuf::from("/ws/b/two")
        );
    }
}
