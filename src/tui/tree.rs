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
    /// Commits are being mirrored to the repo's other remotes
    Syncing,
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
    pub fn new_repo(repo: RepoInfo) -> Self {
        let name = repo
            .display_name
            .split('/')
            .next_back()
            .unwrap_or(&repo.display_name)
            .to_string();
        Self {
            name,
            repo_info: Some(repo),
            children: Vec::new(),
            expanded: false,
        }
    }

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

    // Build tree from regular repos
    for repo in sorted_repos {
        let parts: Vec<&str> = repo.display_name.split('/').collect();

        if parts.is_empty() {
            continue;
        }

        let mut current_level = &mut root_nodes;

        for (i, part) in parts.iter().enumerate() {
            let is_last = i == parts.len() - 1;

            // Find or create node at this level
            let node_idx = current_level.iter().position(|n| n.name == *part);

            if let Some(idx) = node_idx {
                if is_last {
                    // Update existing node with repo info
                    current_level[idx].repo_info = Some(repo.clone());
                }
                current_level = &mut current_level[idx].children;
            } else {
                // Create new node
                let new_node = if is_last {
                    TreeNode::new_repo(repo.clone())
                } else {
                    TreeNode::new_directory((*part).to_string())
                };
                current_level.push(new_node);
                let new_idx = current_level.len() - 1;
                current_level = &mut current_level[new_idx].children;
            }
        }
    }

    // Now insert submodules as children of their parent repos
    for submodule in submodules {
        insert_submodule_into_tree(&mut root_nodes, submodule);
    }

    root_nodes
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
