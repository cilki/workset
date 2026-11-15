use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Default)]
pub enum RepoOperationStatus {
    #[default]
    None,
    /// A background git status scan is running for this repo
    Scanning,
    /// A background sync is fetching the repo's remotes
    Fetching,
    /// The last fetch failed
    SyncFailed(String),
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

    /// Flatten the tree into a list of (node, depth, index_path, full_path)
    /// tuples.
    ///
    /// `full_path` is the names from the root down to this node, joined with
    /// slashes: the path the row actually spells out, and for a repo the same
    /// string the search filter matched it against. Every node's name is
    /// therefore the tail of its own full path, which is what lets the search
    /// highlighter tell the node's characters from its ancestors'. A repo node
    /// used to report its `display_name` here instead — its place on disk,
    /// which is not where the tree puts it once repos are grouped by remote,
    /// and need not even end with the node's name.
    pub fn flatten(
        &self,
        depth: usize,
        index_path: Vec<usize>,
        parent_path: &str,
    ) -> Vec<(&TreeNode, usize, Vec<usize>, String)> {
        let full_path = if parent_path.is_empty() {
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
pub fn build_tree(repos: Vec<RepoInfo>) -> Vec<TreeNode> {
    // Sort repos by modification time (most recent first), repos without one
    // last. Equal times fall back to the name: the rows arrive in whatever
    // order the scan workers finish them, so leaving ties to that order made
    // repos of the same age swap places from one reload to the next — and
    // decided which of two clones of one URL got the remote's node.
    let mut sorted_repos = repos;
    sorted_repos.sort_by(|a, b| {
        let by_time = match (a.modification_time, b.modification_time) {
            (Some(a_time), Some(b_time)) => b_time.cmp(&a_time),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        by_time.then_with(|| a.display_name.cmp(&b.display_name))
    });

    let mut root_nodes: Vec<TreeNode> = Vec::new();

    // Build the tree, grouping repos by remote URL when known. Two clones of
    // the same URL collide on the same node, and the one that loses the node
    // falls back to where it sits on disk so both stay visible.
    for repo in sorted_repos {
        let grouping = repo
            .tree_path
            .clone()
            .unwrap_or_else(|| repo.display_name.clone());
        let mut unplaced = insert_repo_at(&mut root_nodes, &grouping, repo);
        // On-disk paths are distinct, so a repo sent to its own is only ever
        // contested by a clone grouped there from elsewhere, which it
        // displaces; that clone is sent to its own path in turn, and each
        // round leaves one more repo at a path nothing else can claim. The
        // fallback used to be attempted once and its outcome dropped, so a
        // repo whose on-disk path was the node a newer clone of the same URL
        // had already taken vanished from the tree, while the row spelling
        // its path acted on the other clone.
        while let Some(repo) = unplaced {
            let own_path = repo.display_name.clone();
            unplaced = insert_repo_at(&mut root_nodes, &own_path, repo);
            // Only a second repo at this very path could come back out of it,
            // which a workspace can't hold; stop rather than swap the two
            // forever if one ever does
            if unplaced
                .as_ref()
                .is_some_and(|repo| repo.display_name == own_path)
            {
                break;
            }
        }
    }

    root_nodes
}

/// Insert a repo at the slash-separated path, creating directory nodes along
/// the way. When another repo already occupies the target node, the one whose
/// on-disk path is that node keeps it, and the other is handed back to be
/// placed elsewhere.
fn insert_repo_at(root_nodes: &mut Vec<TreeNode>, path: &str, repo: RepoInfo) -> Option<RepoInfo> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }

    let mut current_level = root_nodes;
    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        let node_idx = current_level.iter().position(|n| n.name == *part);

        if let Some(idx) = node_idx {
            if is_last {
                let node = &mut current_level[idx];
                return match node.repo_info.take() {
                    Some(incumbent) if repo.display_name != path => {
                        node.repo_info = Some(incumbent);
                        Some(repo)
                    }
                    displaced => {
                        node.repo_info = Some(repo);
                        displaced
                    }
                };
            }
            current_level = &mut current_level[idx].children;
        } else if is_last {
            current_level.push(TreeNode {
                name: (*part).to_string(),
                repo_info: Some(repo),
                children: Vec::new(),
                expanded: false,
            });
            return None;
        } else {
            current_level.push(TreeNode::new_directory((*part).to_string()));
            let new_idx = current_level.len() - 1;
            current_level = &mut current_level[new_idx].children;
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
        assert!(
            node_at(&tree, &["work", "foo"])
                .unwrap()
                .repo_info
                .is_some()
        );
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

    /// A second clone of a repo already checked out at its remote's path —
    /// `git clone` into a scratch directory, say — groups onto the node the
    /// first one sits at. The newer clone is placed first, and the repo whose
    /// on-disk path that node spells used to fall back to... the same path,
    /// fail again, and vanish: the panel counted one repo fewer than `list`,
    /// and the row reading `github.com/x/y` dropped the scratch clone.
    #[test]
    fn the_repo_at_the_remotes_path_keeps_its_node() {
        let at = |display_name, tree_path, seconds| RepoInfo {
            modification_time: Some(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
            ),
            ..repo(display_name, tree_path)
        };
        let tree = build_tree(vec![
            at("github.com/x/y", Some("github.com/x/y"), 10),
            at("scratch/y-experiment", Some("github.com/x/y"), 20),
        ]);

        assert_eq!(count_repos_in_trees(&tree), 2);
        let home = node_at(&tree, &["github.com", "x", "y"]).unwrap();
        assert_eq!(
            home.repo_info.as_ref().unwrap().display_name,
            "github.com/x/y"
        );
        let scratch = node_at(&tree, &["scratch", "y-experiment"]).unwrap();
        assert_eq!(
            scratch.repo_info.as_ref().unwrap().display_name,
            "scratch/y-experiment"
        );

        // The same when the repo at that path has no remote to group by, so
        // its on-disk path is the only one it ever asks for
        let tree = build_tree(vec![
            at("github.com/x/y", None, 10),
            at("scratch/y-experiment", Some("github.com/x/y"), 20),
        ]);
        assert_eq!(count_repos_in_trees(&tree), 2);
        let home = node_at(&tree, &["github.com", "x", "y"]).unwrap();
        assert_eq!(
            home.repo_info.as_ref().unwrap().display_name,
            "github.com/x/y"
        );
        assert!(node_at(&tree, &["scratch", "y-experiment"]).is_some());
    }

    /// A displaced clone falls back to its own path, which may itself be the
    /// node another repo grouped onto, and so on; every repo ends up somewhere
    #[test]
    fn displaced_repos_make_room_for_each_other() {
        let at = |display_name, tree_path, seconds| RepoInfo {
            modification_time: Some(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
            ),
            ..repo(display_name, tree_path)
        };
        // Newest first: a takes q/b, b takes r/c, then c claims r/c and
        // displaces b to q/b, which displaces a to p/a
        let tree = build_tree(vec![
            at("p/a", Some("q/b"), 30),
            at("q/b", Some("r/c"), 20),
            at("r/c", None, 10),
        ]);

        assert_eq!(count_repos_in_trees(&tree), 3);
        for path in ["p/a", "q/b", "r/c"] {
            let parts: Vec<&str> = path.split('/').collect();
            let node = node_at(&tree, &parts).unwrap();
            assert_eq!(node.repo_info.as_ref().unwrap().display_name, path);
        }
    }

    /// The scan hands rows over in the order its workers finish, which
    /// differs from one reload to the next. Repos of the same age — two clones
    /// of one URL at the same commit, say — must come out the same way from
    /// any of those orders.
    #[test]
    fn repos_of_the_same_age_are_ordered_by_name() {
        let at = |display_name, tree_path, seconds| RepoInfo {
            modification_time: Some(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
            ),
            ..repo(display_name, tree_path)
        };
        let rows = || {
            vec![
                at("work/old", None, 10),
                at("b/two", Some("github.com/x/y"), 20),
                at("work/zeta", None, 20),
                at("a/one", Some("github.com/x/y"), 20),
                at("work/alpha", None, 20),
                at("work/new", None, 30),
            ]
        };
        let order = |tree: &[TreeNode]| -> Vec<String> {
            flatten_trees(tree)
                .into_iter()
                .filter_map(|(node, ..)| Some(node.repo_info.as_ref()?.display_name.clone()))
                .collect()
        };

        let forward = build_tree(rows());
        let mut reversed_rows = rows();
        reversed_rows.reverse();
        let reversed = build_tree(reversed_rows);

        assert_eq!(order(&forward), order(&reversed));
        // Newest first, ties by name, within each directory; the first clone
        // by name owns the remote's node and the other falls back to where it
        // sits on disk
        assert_eq!(
            order(&forward),
            [
                "work/new",
                "work/alpha",
                "work/zeta",
                "work/old",
                "a/one",
                "b/two"
            ]
        );
        let primary = node_at(&forward, &["github.com", "x", "y"]).unwrap();
        assert_eq!(primary.repo_info.as_ref().unwrap().display_name, "a/one");
    }
}
