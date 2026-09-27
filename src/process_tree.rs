//! Process hierarchy built entirely from one bulk snapshot.
//!
//! A recorded parent PID is only a hint: Windows can reuse it after the original
//! parent exits. A child is attached only to an unambiguous parent whose creation
//! time is known and no later than its own. No process handles are opened here.

use std::collections::{HashMap, HashSet};

use crate::{i18n::tr, sampler::Process};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Identity {
    pub pid: u32,
    pub created: u64,
}

impl From<&Process> for Identity {
    fn from(process: &Process) -> Self {
        Self {
            pid: process.pid,
            created: process.created,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row {
    pub index: usize,
    pub depth: usize,
    pub has_children: bool,
    pub expanded: bool,
    /// Children shown under this row when expanded (after filtering).
    pub children: usize,
}

/// Immutable, children-first identities captured before the confirmation dialog.
/// Later snapshots and newly created descendants never enlarge this action.
#[derive(Clone, Debug)]
pub struct TerminationPlan {
    pub root: Identity,
    pub root_name: String,
    targets: Vec<Identity>,
}

impl TerminationPlan {
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn targets(&self) -> &[Identity] {
        &self.targets
    }
}

pub struct Tree<'a> {
    processes: &'a [Process],
    parents: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
    ambiguous: HashSet<u32>,
}

impl<'a> Tree<'a> {
    pub fn new(processes: &'a [Process]) -> Self {
        let mut by_pid = HashMap::with_capacity(processes.len());
        let mut ambiguous = HashSet::new();
        for (index, process) in processes.iter().enumerate() {
            if by_pid.insert(process.pid, index).is_some() {
                ambiguous.insert(process.pid);
            }
        }
        let mut parents: Vec<_> = processes
            .iter()
            .enumerate()
            .map(|(index, child)| {
                let parent_index = *by_pid.get(&child.parent_pid)?;
                let parent = &processes[parent_index];
                (parent_index != index
                    && !ambiguous.contains(&child.pid)
                    && !ambiguous.contains(&parent.pid)
                    && parent.created != 0
                    && child.created != 0
                    && parent.created <= child.created)
                    .then_some(parent_index)
            })
            .collect();

        // Equal timestamps or malformed synthetic input can still contain a
        // cycle. Discard every edge in that cycle; iterative walks avoid stack
        // overflow even for a hostile or unusually deep hierarchy.
        let mut state = vec![0u8; processes.len()];
        let mut path = Vec::new();
        for start in 0..processes.len() {
            if state[start] != 0 {
                continue;
            }
            path.clear();
            let mut cursor = Some(start);
            while let Some(index) = cursor {
                match state[index] {
                    0 => {
                        state[index] = 1;
                        path.push(index);
                        cursor = parents[index];
                    }
                    1 => {
                        let cycle_start = path.iter().position(|&entry| entry == index).unwrap();
                        for &entry in &path[cycle_start..] {
                            parents[entry] = None;
                        }
                        break;
                    }
                    _ => break,
                }
            }
            for &index in &path {
                state[index] = 2;
            }
        }
        let mut children = vec![Vec::new(); processes.len()];
        for (child, parent) in parents.iter().enumerate() {
            if let Some(parent) = parent {
                children[*parent].push(child);
            }
        }
        Self {
            processes,
            parents,
            children,
            ambiguous,
        }
    }

    /// The parent of `index` in the hierarchy (None for roots and unknown
    /// indices).
    pub fn parent(&self, index: usize) -> Option<usize> {
        self.parents.get(index).copied().flatten()
    }

    /// The children of `index` (snapshot indices, in snapshot order).
    pub fn children_of(&self, index: usize) -> &[usize] {
        self.children.get(index).map_or(&[], Vec::as_slice)
    }

    /// `sorted_indices` gives the desired sibling/root order, normally all
    /// snapshot indices sorted by the active column. Missing indices are appended
    /// in snapshot order; duplicates and out-of-range indices are ignored.
    /// Filtering retains matching processes and their ancestors, and temporarily
    /// opens those ancestor paths without mutating the saved collapse state.
    pub fn rows(
        &self,
        sorted_indices: &[usize],
        collapsed: &HashSet<Identity>,
        matches: Option<&HashSet<usize>>,
    ) -> Vec<Row> {
        let count = self.processes.len();
        let mut included = vec![matches.is_none(); count];
        if let Some(matches) = matches {
            for &matched in matches {
                let mut cursor = (matched < count).then_some(matched);
                while let Some(index) = cursor {
                    if included[index] {
                        break;
                    }
                    included[index] = true;
                    cursor = self.parents[index];
                }
            }
        }

        let mut roots = Vec::new();
        let mut children = vec![Vec::new(); count];
        let mut seen = vec![false; count];
        for index in sorted_indices.iter().copied().chain(0..count) {
            if index >= count || seen[index] || !included[index] {
                continue;
            }
            seen[index] = true;
            match self.parents[index] {
                Some(parent) => children[parent].push(index),
                None => roots.push(index),
            }
        }
        let mut rows = Vec::with_capacity(count);
        let mut stack: Vec<_> = roots.into_iter().rev().map(|index| (index, 0)).collect();
        while let Some((index, depth)) = stack.pop() {
            let has_children = !children[index].is_empty();
            let expanded = has_children
                && (matches.is_some()
                    || !collapsed.contains(&Identity::from(&self.processes[index])));
            rows.push(Row {
                index,
                depth,
                has_children,
                expanded,
                children: children[index].len(),
            });
            if expanded {
                stack.extend(
                    children[index]
                        .iter()
                        .rev()
                        .map(|&child| (child, depth + 1)),
                );
            }
        }
        rows
    }

    pub fn plan(&self, root_index: usize) -> Result<TerminationPlan, String> {
        let root = self.processes.get(root_index).ok_or_else(|| {
            tr(
                "선택한 프로세스가 없습니다. 목록을 새로 고치세요.",
                "The selected process is unavailable. Refresh the list.",
            )
            .to_owned()
        })?;
        let mut targets = Vec::new();
        let mut stack = vec![(root_index, false)];
        while let Some((index, visited)) = stack.pop() {
            let process = &self.processes[index];
            if process.created == 0 || self.ambiguous.contains(&process.pid) {
                return Err(tr(
                    "트리에 시작 시간을 확인할 수 없는 프로세스가 있습니다. 새로 고친 후 다시 시도하세요.",
                    "A process in this tree has an unverified identity. Refresh the list and try again.",
                ).into());
            }
            if visited {
                targets.push(Identity::from(process));
            } else {
                stack.push((index, true));
                stack.extend(
                    self.children[index]
                        .iter()
                        .rev()
                        .map(|&child| (child, false)),
                );
            }
        }
        Ok(TerminationPlan {
            root: Identity::from(root),
            root_name: root.name.clone(),
            targets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, parent_pid: u32, created: u64) -> Process {
        Process {
            pid,
            parent_pid,
            created,
            name: format!("process-{pid}"),
            cpu_percent: 0.0,
            working_set: 0,
            private_bytes: 0,
            io_bytes_per_sec: 0.0,
            gpu_percent: None,
            network_bytes_per_sec: None,
            threads: 1,
            handles: 0,
            ..Process::default()
        }
    }

    fn indices(rows: &[Row]) -> Vec<usize> {
        rows.iter().map(|row| row.index).collect()
    }

    #[test]
    fn roots_and_siblings_follow_caller_sort_order() {
        let processes = vec![
            process(10, 0, 10),
            process(20, 10, 20),
            process(30, 10, 30),
            process(40, 0, 40),
        ];
        let tree = Tree::new(&processes);
        let rows = tree.rows(&[3, 2, 1, 0], &HashSet::new(), None);
        assert_eq!(indices(&rows), [3, 0, 2, 1]);
        assert_eq!(
            rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
            [0, 0, 1, 1]
        );
        assert!(rows[1].has_children && rows[1].expanded);
    }

    #[test]
    fn reused_parent_and_unknown_creation_do_not_capture_unrelated_children() {
        let processes = vec![
            process(10, 0, 100),
            process(20, 10, 20),
            process(30, 10, 130),
            process(40, 10, 0),
            process(50, 999, 50),
        ];
        let tree = Tree::new(&processes);
        assert_eq!(
            tree.plan(0).unwrap().targets(),
            &[Identity::from(&processes[2]), Identity::from(&processes[0])]
        );
        let rows = tree.rows(&[], &HashSet::new(), None);
        assert_eq!(rows.iter().filter(|row| row.depth == 0).count(), 4);
        assert!(tree.plan(3).is_err());
    }

    #[test]
    fn filtering_retains_ancestors_and_temporarily_opens_collapsed_paths() {
        let processes = vec![
            process(10, 0, 10),
            process(20, 10, 20),
            process(30, 20, 30),
            process(40, 10, 40),
        ];
        let tree = Tree::new(&processes);
        let collapsed = HashSet::from([Identity::from(&processes[0])]);
        assert_eq!(indices(&tree.rows(&[], &collapsed, None)), [0]);
        let rows = tree.rows(&[], &collapsed, Some(&HashSet::from([2])));
        assert_eq!(indices(&rows), [0, 1, 2]);
        assert!(rows[0].expanded && rows[1].expanded);
        assert_eq!(collapsed.len(), 1);
        assert!(tree.rows(&[], &collapsed, Some(&HashSet::new())).is_empty());
        // Filtering or collapsing never narrows a subsequent full-tree action.
        assert_eq!(tree.plan(0).unwrap().len(), 4);
    }

    #[test]
    fn collapse_identity_does_not_follow_reused_pid() {
        let processes = vec![process(10, 0, 100), process(20, 10, 120)];
        let stale = HashSet::from([Identity {
            pid: 10,
            created: 10,
        }]);
        assert_eq!(Tree::new(&processes).rows(&[], &stale, None).len(), 2);
    }

    #[test]
    fn malformed_cycles_duplicate_pids_and_sort_indices_are_bounded() {
        let processes = vec![
            process(10, 20, 10),
            process(20, 10, 10),
            process(30, 30, 30),
            process(40, 10, 40),
            process(40, 10, 40),
            process(50, 40, 50),
        ];
        let tree = Tree::new(&processes);
        let rows = tree.rows(&[999, 5, 5, 0], &HashSet::new(), None);
        assert_eq!(rows.len(), processes.len());
        assert_eq!(
            indices(&rows).into_iter().collect::<HashSet<_>>().len(),
            processes.len()
        );
        assert!(tree.plan(3).is_err());
        assert_eq!(tree.plan(5).unwrap().len(), 1);
        assert!(tree.plan(999).is_err());
    }

    #[test]
    fn termination_plan_is_children_first_and_independent_of_later_snapshots() {
        let mut processes = vec![process(10, 0, 10), process(20, 10, 20), process(30, 20, 30)];
        let plan = Tree::new(&processes).plan(0).unwrap();
        assert_eq!(
            plan.targets()
                .iter()
                .map(|identity| identity.pid)
                .collect::<Vec<_>>(),
            [30, 20, 10]
        );
        processes.push(process(40, 10, 40));
        processes[1].created = 200;
        assert_eq!(plan.len(), 3);
        assert_eq!(plan.targets()[1].created, 20);
        assert_eq!(plan.root.pid, 10);
        assert_eq!(plan.root_name, "process-10");
    }

    #[test]
    fn deep_tree_uses_no_recursive_walks() {
        let processes: Vec<_> = (1..=10_000)
            .map(|pid| process(pid, pid - 1, u64::from(pid)))
            .collect();
        let tree = Tree::new(&processes);
        let rows = tree.rows(&[], &HashSet::new(), None);
        assert_eq!(rows.last().unwrap().depth, 9_999);
        assert_eq!(tree.plan(0).unwrap().len(), 10_000);
    }
}
