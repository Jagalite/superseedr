// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The raw data structure (The "Truth")
#[derive(Debug, Clone, PartialEq)]
pub struct RawNode<T> {
    pub name: String,
    pub full_path: PathBuf, // Must match the crawler output in persistence/payload.rs
    pub children: Vec<RawNode<T>>,
    pub payload: T,
    pub is_dir: bool,
}

// ------------------------------------------------------------------
// BLOCK 1: General methods (Relaxed bounds)
// These methods work for any T that can be Cloned.
// ------------------------------------------------------------------
impl<T: Clone> RawNode<T> {
    pub fn expand_all(&self, state: &mut TreeViewState) {
        if self.is_dir {
            state.expanded_paths.insert(self.full_path.clone());
            for child in &self.children {
                child.expand_all(state);
            }
        }
    }

    pub fn sort_recursive(&mut self) {
        self.children.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        for child in &mut self.children {
            child.sort_recursive();
        }
    }

    pub fn find_and_act<F>(&mut self, target_path: &Path, action: &mut F) -> bool
    where
        F: FnMut(&mut Self),
    {
        if self.full_path == target_path {
            action(self);
            return true;
        }
        if target_path.starts_with(&self.full_path) {
            for child in &mut self.children {
                if child.find_and_act(target_path, action) {
                    return true;
                }
            }
        }
        false
    }

    pub fn apply_recursive<F>(&mut self, action: &F)
    where
        F: Fn(&mut Self),
    {
        action(self);
        for child in &mut self.children {
            child.apply_recursive(action);
        }
    }
}

// ------------------------------------------------------------------
// BLOCK 2: Math-heavy methods (Strict bounds)
// These explicitly require T to be summable (AddAssign).
// ------------------------------------------------------------------
impl<T: Clone + Default + std::ops::AddAssign> RawNode<T> {
    pub fn from_path_list(custom_root: Option<String>, files: Vec<(Vec<String>, T)>) -> Vec<Self> {
        let mut internal_root = RawNode {
            name: String::new(),
            full_path: PathBuf::new(),
            children: Vec::new(),
            payload: T::default(),
            is_dir: true,
        };

        for (path_parts, payload) in files {
            let _ = internal_root.insert_recursive(&path_parts, payload, Path::new(""));
        }

        internal_root.sort_recursive();

        if let Some(root_name) = custom_root {
            let wrapper = RawNode {
                name: root_name.clone(),
                full_path: PathBuf::from(root_name),
                children: internal_root.children,
                payload: internal_root.payload,
                is_dir: true,
            };
            vec![wrapper]
        } else {
            internal_root.children
        }
    }

    fn insert_recursive(&mut self, path_parts: &[String], payload: T, parent_path: &Path) -> bool {
        if path_parts.is_empty() {
            return false;
        }

        let name = &path_parts[0];
        let is_last = path_parts.len() == 1;
        let current_path = parent_path.join(name);

        let (child_idx, created) =
            if let Some(idx) = self.children.iter().position(|child| &child.name == name) {
                // A valid path list cannot contain an exact duplicate or use the same path as both
                // a file and a directory. Keep the first structurally valid entry if malformed data
                // reaches this generic helper instead of overwriting its payload or hiding children.
                if is_last || !self.children[idx].is_dir {
                    tracing::warn!(
                        path = %current_path.display(),
                        "Ignoring duplicate or prefix-colliding tree entry"
                    );
                    return false;
                }
                (idx, false)
            } else {
                let new_node = RawNode {
                    name: name.clone(),
                    full_path: current_path.clone(),
                    children: Vec::new(),
                    payload: T::default(),
                    is_dir: !is_last,
                };
                self.children.push(new_node);
                (self.children.len() - 1, true)
            };

        if is_last {
            self.children[child_idx].payload = payload.clone();
            self.payload += payload;
            return true;
        }

        if self.children[child_idx].insert_recursive(
            &path_parts[1..],
            payload.clone(),
            &current_path,
        ) {
            self.payload += payload;
            true
        } else {
            if created {
                self.children.remove(child_idx);
            }
            false
        }
    }
}

impl RawNode<crate::app::TorrentPreviewPayload> {
    /// Recursively collects all file indices and their associated priorities.
    /// This is used when confirming a download to pass the user's selection to the engine.
    pub fn collect_priorities(
        &self,
        out: &mut std::collections::HashMap<usize, crate::app::FilePriority>,
    ) {
        // If this node is a file (has an index), record its priority
        if let Some(idx) = self.payload.file_index {
            out.insert(idx, self.payload.priority);
        }

        // Recurse through all children
        for child in &self.children {
            child.collect_priorities(out);
        }
    }
}

type FilterRule<T> = Rc<dyn Fn(&RawNode<T>) -> bool>;

#[derive(Clone)]
pub struct TreeFilter<T> {
    pub queries: Vec<String>,
    pub node_rule: Option<FilterRule<T>>,
    pub match_name: bool,
    pub auto_expand: bool,
}

impl<T> Default for TreeFilter<T> {
    fn default() -> Self {
        Self {
            queries: Vec::new(),
            node_rule: None,
            match_name: true,
            auto_expand: false,
        }
    }
}

impl<T> TreeFilter<T> {
    pub fn from_text(input: &str) -> Self {
        let queries: Vec<String> = input
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect();
        let auto_expand = !queries.is_empty();
        Self {
            queries,
            node_rule: None,
            match_name: true,
            auto_expand,
        }
    }

    pub fn new(input: &str, rule: impl Fn(&RawNode<T>) -> bool + 'static) -> Self {
        let mut filter = Self::from_text(input);
        filter.node_rule = Some(Rc::new(rule));
        filter
    }

    pub fn rule_only(input: &str, rule: impl Fn(&RawNode<T>) -> bool + 'static) -> Self {
        let mut filter = Self::from_text(input);
        filter.match_name = false;
        filter.node_rule = Some(Rc::new(rule));
        filter
    }

    pub fn matches(&self, node: &RawNode<T>) -> bool {
        if let Some(rule) = &self.node_rule {
            if !(rule)(node) {
                return false;
            }
        }
        if self.queries.is_empty() || !self.match_name {
            return true;
        }
        let name_lower = node.name.to_lowercase();
        self.queries.iter().all(|q| name_lower.contains(q))
    }

    pub fn any_matches(&self, node: &RawNode<T>) -> bool {
        if self.matches(node) {
            return true;
        }
        node.children.iter().any(|child| self.any_matches(child))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TreeViewState {
    pub cursor_path: Option<PathBuf>,
    pub current_path: PathBuf,
    pub expanded_paths: HashSet<PathBuf>,
    pub selected_paths: HashSet<PathBuf>,
    pub top_most_offset: usize,
}

impl TreeViewState {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, PartialEq)]
pub struct RenderItem<'a, T> {
    pub node: &'a RawNode<T>,
    pub path: PathBuf,
    pub depth: usize,
    pub is_last: bool,
    pub is_expanded: bool,
    pub is_selected: bool,
    pub is_cursor: bool,
}

impl<'a, T> Clone for RenderItem<'a, T> {
    fn clone(&self) -> Self {
        Self {
            node: self.node,
            path: self.path.clone(),
            depth: self.depth,
            is_last: self.is_last,
            is_expanded: self.is_expanded,
            is_selected: self.is_selected,
            is_cursor: self.is_cursor,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TreeAction {
    Up,
    Down,
    Left,
    Right,
}

pub struct TreeMathHelper;

impl TreeMathHelper {
    pub fn get_visible_slice<'a, T>(
        nodes: &'a [RawNode<T>],
        state: &TreeViewState,
        filter: TreeFilter<T>,
        max_height: usize,
    ) -> Vec<RenderItem<'a, T>> {
        TreeProjection::new(nodes, state, filter, max_height)
            .visible_window()
            .to_vec()
    }

    pub fn apply_action<T>(
        state: &mut TreeViewState,
        nodes: &[RawNode<T>],
        action: TreeAction,
        filter: TreeFilter<T>,
        max_height: usize,
    ) -> bool {
        TreeProjection::new(nodes, state, filter, max_height).apply_action(state, action)
    }
}

pub struct TreeProjection<'a, T> {
    rows: Vec<RenderItem<'a, T>>,
    window_start: usize,
    window_end: usize,
    max_height: usize,
}

impl<'a, T> TreeProjection<'a, T> {
    pub fn new(
        nodes: &'a [RawNode<T>],
        state: &TreeViewState,
        filter: TreeFilter<T>,
        max_height: usize,
    ) -> Self {
        let mut full_list = Vec::new();
        Self::project_recursive(nodes, state, &filter, 0, &mut full_list);

        let effective_height = max_height.max(1);
        let max_start = full_list.len().saturating_sub(effective_height);
        let window_start = state.top_most_offset.min(max_start);
        let window_end = (window_start + max_height).min(full_list.len());

        Self {
            rows: full_list,
            window_start,
            window_end,
            max_height,
        }
    }

    #[cfg(test)]
    pub fn rows(&self) -> &[RenderItem<'a, T>] {
        &self.rows
    }

    pub fn visible_window(&self) -> &[RenderItem<'a, T>] {
        if self.window_start < self.window_end {
            &self.rows[self.window_start..self.window_end]
        } else {
            &[]
        }
    }

    pub fn cursor_index(&self, state: &TreeViewState) -> Option<usize> {
        state
            .cursor_path
            .as_ref()
            .and_then(|path| self.rows.iter().position(|item| &item.path == path))
    }

    pub fn apply_action(&self, state: &mut TreeViewState, action: TreeAction) -> bool {
        self.handle_action(state, action)
    }

    fn project_recursive(
        nodes: &'a [RawNode<T>],
        state: &TreeViewState,
        filter: &TreeFilter<T>,
        depth: usize,
        output: &mut Vec<RenderItem<'a, T>>,
    ) {
        let visible_nodes: Vec<_> = nodes
            .iter()
            .filter(|node| filter.any_matches(node))
            .collect();

        let len = visible_nodes.len();
        for (i, node) in visible_nodes.into_iter().enumerate() {
            let path = node.full_path.clone();
            let expanded = if filter.auto_expand {
                true
            } else {
                state.expanded_paths.contains(&path)
            };

            output.push(RenderItem {
                node,
                path: path.clone(),
                depth,
                is_last: i == len - 1,
                is_expanded: expanded,
                is_selected: state.selected_paths.contains(&path),
                is_cursor: state.cursor_path.as_ref() == Some(&path),
            });

            if node.is_dir && expanded {
                Self::project_recursive(&node.children, state, filter, depth + 1, output);
            }
        }
    }

    fn handle_action(&self, state: &mut TreeViewState, action: TreeAction) -> bool {
        if self.rows.is_empty() {
            return false;
        }

        let Some(current_idx) = self.cursor_index(state) else {
            state.cursor_path = Some(self.rows[0].path.clone());
            self.keep_cursor_visible(state, 0);
            return true;
        };

        let mut new_idx = current_idx;

        match action {
            TreeAction::Up => new_idx = current_idx.saturating_sub(1),
            TreeAction::Down => {
                if current_idx < self.rows.len() - 1 {
                    new_idx = current_idx + 1;
                }
            }
            TreeAction::Right => {
                let item = &self.rows[current_idx];
                if item.node.is_dir {
                    if !state.expanded_paths.contains(&item.path) {
                        state.expanded_paths.insert(item.path.clone());
                    } else if current_idx < self.rows.len() - 1 {
                        let next_item = &self.rows[current_idx + 1];
                        if next_item.depth > item.depth {
                            new_idx = current_idx + 1;
                        }
                    }
                }
            }
            TreeAction::Left => {
                let item = &self.rows[current_idx];
                if item.node.is_dir && state.expanded_paths.contains(&item.path) {
                    state.expanded_paths.remove(&item.path);
                } else if item.depth > 0 {
                    let parent = self.rows[0..current_idx]
                        .iter()
                        .rfind(|x| x.depth == item.depth - 1);
                    if let Some(p) = parent {
                        new_idx = self
                            .rows
                            .iter()
                            .position(|i| i.path == p.path)
                            .unwrap_or(current_idx);
                    }
                }
            }
        }

        state.cursor_path = Some(self.rows[new_idx].path.clone());
        self.keep_cursor_visible(state, new_idx);
        true
    }

    fn keep_cursor_visible(&self, state: &mut TreeViewState, new_idx: usize) {
        let effective_height = self.max_height.max(1);
        if new_idx < state.top_most_offset {
            state.top_most_offset = new_idx;
        } else if new_idx >= state.top_most_offset + effective_height {
            state.top_most_offset = (new_idx + 1).saturating_sub(effective_height);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct TestPayload {
        progress: f64,
    }

    fn mock_complex_tree() -> Vec<RawNode<TestPayload>> {
        vec![
            RawNode {
                name: "root1".to_string(),
                full_path: PathBuf::from("root1"),
                is_dir: true,
                payload: TestPayload { progress: 0.0 },
                children: vec![
                    RawNode {
                        name: "sub1".to_string(),
                        full_path: PathBuf::from("root1/sub1"),
                        is_dir: true,
                        payload: TestPayload { progress: 0.0 },
                        children: vec![
                            RawNode {
                                name: "leaf1".to_string(),
                                full_path: PathBuf::from("root1/sub1/leaf1"),
                                is_dir: false,
                                payload: TestPayload { progress: 1.0 },
                                children: vec![],
                            },
                            RawNode {
                                name: "leaf2".to_string(),
                                full_path: PathBuf::from("root1/sub1/leaf2"),
                                is_dir: false,
                                payload: TestPayload { progress: 1.0 },
                                children: vec![],
                            },
                        ],
                    },
                    RawNode {
                        name: "leaf3".to_string(),
                        full_path: PathBuf::from("root1/leaf3"),
                        is_dir: false,
                        payload: TestPayload { progress: 1.0 },
                        children: vec![],
                    },
                ],
            },
            RawNode {
                name: "root_leaf".to_string(),
                full_path: PathBuf::from("root_leaf"),
                is_dir: false,
                payload: TestPayload { progress: 1.0 },
                children: vec![],
            },
        ]
    }

    #[test]
    fn test_initial_state() {
        let tree = mock_complex_tree();
        let state = TreeViewState::default();
        let list = TreeMathHelper::get_visible_slice(&tree, &state, TreeFilter::from_text(""), 10);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_scrolling_down_triggers_offset() {
        let tree = mock_complex_tree();
        let mut state = TreeViewState::default();
        state.expanded_paths.insert(PathBuf::from("root1"));

        let max_height = 2;
        state.cursor_path = Some(PathBuf::from("root1"));

        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Down,
            TreeFilter::from_text(""),
            max_height,
        );
        assert_eq!(state.top_most_offset, 0);

        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Down,
            TreeFilter::from_text(""),
            max_height,
        );
        assert_eq!(state.top_most_offset, 1);
    }

    #[test]
    fn test_scrolling_behavior_on_zoom_change() {
        let tree = mock_complex_tree();
        let mut state = TreeViewState::default();
        state.expanded_paths.insert(PathBuf::from("root1"));

        state.cursor_path = Some(PathBuf::from("root_leaf"));
        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Left,
            TreeFilter::from_text(""),
            10,
        );
        assert_eq!(state.top_most_offset, 0);
    }

    #[test]
    fn test_left_collapses_dir() {
        let tree = mock_complex_tree();
        let mut state = TreeViewState::default();
        let path = PathBuf::from("root1");
        state.expanded_paths.insert(path.clone());
        state.cursor_path = Some(path.clone());

        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Left,
            TreeFilter::from_text(""),
            10,
        );
        assert!(!state.expanded_paths.contains(&path));
    }

    #[test]
    fn test_search_auto_expands_and_filters() {
        let tree = mock_complex_tree();
        let state = TreeViewState::default();

        let list =
            TreeMathHelper::get_visible_slice(&tree, &state, TreeFilter::from_text("leaf1"), 10);

        assert_eq!(list.len(), 3);
        assert!(list[0].is_expanded);
        assert!(list[1].is_expanded);
        assert_eq!(list[2].node.name, "leaf1");
    }

    #[test]
    fn test_projection_exposes_full_rows_and_visible_window() {
        let tree = mock_complex_tree();
        let state = TreeViewState {
            top_most_offset: 1,
            ..Default::default()
        };

        let projection = TreeProjection::new(&tree, &state, TreeFilter::from_text("leaf1"), 2);

        assert_eq!(projection.rows().len(), 3);
        assert_eq!(projection.visible_window().len(), 2);
        assert_eq!(projection.visible_window()[0].node.name, "sub1");
        assert_eq!(projection.visible_window()[1].node.name, "leaf1");
    }

    #[test]
    fn test_navigation_with_stale_cursor_selects_first_visible_row() {
        let tree = mock_complex_tree();
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("missing")),
            ..Default::default()
        };

        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Down,
            TreeFilter::from_text(""),
            10,
        );

        assert_eq!(state.cursor_path, Some(PathBuf::from("root1")));
    }

    #[test]
    fn test_search_clamps_stale_scroll_offset() {
        let tree = mock_complex_tree();
        let state = TreeViewState {
            top_most_offset: 20,
            ..Default::default()
        };

        let list =
            TreeMathHelper::get_visible_slice(&tree, &state, TreeFilter::from_text("leaf1"), 10);

        assert_eq!(list.len(), 3);
        assert_eq!(list[2].node.name, "leaf1");
    }

    #[test]
    fn test_lazy_loading_simulation() {
        let mut tree = vec![RawNode {
            name: "photos".to_string(),
            full_path: PathBuf::from("photos"),
            is_dir: true,
            payload: TestPayload { progress: 0.0 },
            children: vec![],
        }];
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("photos")),
            ..Default::default()
        };

        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Right,
            TreeFilter::from_text(""),
            10,
        );

        assert!(state.expanded_paths.contains(&PathBuf::from("photos")));
        let visible =
            TreeMathHelper::get_visible_slice(&tree, &state, TreeFilter::from_text(""), 10);
        assert_eq!(visible.len(), 1);

        tree[0].children.push(RawNode {
            name: "vacation.jpg".to_string(),
            full_path: PathBuf::from("photos/vacation.jpg"),
            is_dir: false,
            payload: TestPayload { progress: 1.0 },
            children: vec![],
        });

        let visible_after_load =
            TreeMathHelper::get_visible_slice(&tree, &state, TreeFilter::from_text(""), 10);
        assert_eq!(visible_after_load.len(), 2);
        assert_eq!(visible_after_load[1].node.name, "vacation.jpg");
    }

    #[test]
    fn test_list_reordering_preserves_cursor() {
        let mut tree = vec![
            RawNode {
                name: "Slow".into(),
                full_path: PathBuf::from("Slow"),
                is_dir: false,
                payload: TestPayload { progress: 0.0 },
                children: vec![],
            },
            RawNode {
                name: "Fast".into(),
                full_path: PathBuf::from("Fast"),
                is_dir: false,
                payload: TestPayload { progress: 0.0 },
                children: vec![],
            },
        ];
        let state = TreeViewState {
            cursor_path: Some(PathBuf::from("Fast")),
            ..Default::default()
        };

        tree.swap(0, 1);

        let visible =
            TreeMathHelper::get_visible_slice(&tree, &state, TreeFilter::from_text(""), 10);
        assert_eq!(visible[0].node.name, "Fast");
        assert!(visible[0].is_cursor);
        assert!(!visible[1].is_cursor);
    }

    #[test]
    fn test_expanding_actually_empty_directory() {
        let tree = vec![RawNode {
            name: "EmptyDir".into(),
            full_path: PathBuf::from("EmptyDir"),
            is_dir: true,
            payload: TestPayload { progress: 0.0 },
            children: vec![],
        }];
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("EmptyDir")),
            ..Default::default()
        };

        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Right,
            TreeFilter::from_text(""),
            10,
        );
        assert!(state.expanded_paths.contains(&PathBuf::from("EmptyDir")));

        let old_cursor = state.cursor_path.clone();
        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Right,
            TreeFilter::from_text(""),
            10,
        );
        assert_eq!(state.cursor_path, old_cursor);
    }

    #[test]
    fn test_smart_nav_right_descends_into_child() {
        let tree = vec![RawNode {
            name: "Root".into(),
            full_path: PathBuf::from("Root"),
            is_dir: true,
            payload: TestPayload { progress: 0.0 },
            children: vec![RawNode {
                name: "Child".into(),
                full_path: PathBuf::from("Root/Child"),
                is_dir: false,
                payload: TestPayload { progress: 0.0 },
                children: vec![],
            }],
        }];
        let mut state = TreeViewState::default();
        state.expanded_paths.insert(PathBuf::from("Root"));
        state.cursor_path = Some(PathBuf::from("Root"));

        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Right,
            TreeFilter::from_text(""),
            10,
        );
        assert_eq!(state.cursor_path, Some(PathBuf::from("Root/Child")));
    }

    #[test]
    fn test_smart_nav_left_jumps_to_parent() {
        let tree = vec![RawNode {
            name: "Root".into(),
            full_path: PathBuf::from("Root"),
            is_dir: true,
            payload: TestPayload { progress: 0.0 },
            children: vec![RawNode {
                name: "Child".into(),
                full_path: PathBuf::from("Root/Child"),
                is_dir: false,
                payload: TestPayload { progress: 0.0 },
                children: vec![],
            }],
        }];
        let mut state = TreeViewState::default();
        state.expanded_paths.insert(PathBuf::from("Root"));
        state.cursor_path = Some(PathBuf::from("Root/Child"));

        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Left,
            TreeFilter::from_text(""),
            10,
        );
        assert_eq!(state.cursor_path, Some(PathBuf::from("Root")));
        assert!(state.expanded_paths.contains(&PathBuf::from("Root")));
    }

    #[test]
    fn test_selection_persists_after_collapse() {
        let tree = vec![RawNode {
            name: "Root".into(),
            full_path: PathBuf::from("Root"),
            is_dir: true,
            payload: TestPayload { progress: 0.0 },
            children: vec![RawNode {
                name: "Child".into(),
                full_path: PathBuf::from("Root/Child"),
                is_dir: false,
                payload: TestPayload { progress: 0.0 },
                children: vec![],
            }],
        }];
        let mut state = TreeViewState::default();
        let child_path = PathBuf::from("Root/Child");

        state.expanded_paths.insert(PathBuf::from("Root"));
        state.selected_paths.insert(child_path.clone());
        state.cursor_path = Some(PathBuf::from("Root"));

        TreeMathHelper::apply_action(
            &mut state,
            &tree,
            TreeAction::Left,
            TreeFilter::from_text(""),
            10,
        );
        assert!(!state.expanded_paths.contains(&PathBuf::from("Root")));
        assert!(state.selected_paths.contains(&child_path));
    }

    #[test]
    fn duplicate_and_prefix_collisions_keep_the_first_valid_tree_entry() {
        let duplicate_tree = RawNode::from_path_list(
            Some("root".to_string()),
            vec![
                (vec!["entry".to_string()], 5u64),
                (vec!["entry".to_string()], 9u64),
            ],
        );
        assert_eq!(duplicate_tree[0].payload, 5);
        assert_eq!(duplicate_tree[0].children.len(), 1);
        assert_eq!(duplicate_tree[0].children[0].payload, 5);

        let prefix_tree = RawNode::from_path_list(
            Some("root".to_string()),
            vec![
                (vec!["entry".to_string()], 5u64),
                (vec!["entry".to_string(), "child".to_string()], 9u64),
            ],
        );
        assert_eq!(prefix_tree[0].payload, 5);
        assert_eq!(prefix_tree[0].children.len(), 1);
        assert!(!prefix_tree[0].children[0].is_dir);
        assert!(prefix_tree[0].children[0].children.is_empty());
    }
}
