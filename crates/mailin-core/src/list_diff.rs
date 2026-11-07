//! Pure list diff by stable ID (M3b-1 prep).
//!
//! Produces a plan to reconcile an existing row list with a new message list
//! without clearing and rebuilding everything. UI code applies the plan
//! (platform-specific). Unit-testable on Linux.

use std::collections::HashMap;
use std::hash::Hash;

/// Reconciliation plan: for each item in `new`, which old index to reuse (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPlan {
    /// `reuse[new_index] = Some(old_index)` when the item can be reused.
    pub reuse: Vec<Option<usize>>,
    /// Old indices not present in `new`, sorted descending for safe removal.
    pub removed_desc: Vec<usize>,
}

impl DiffPlan {
    pub fn inserted_count(&self) -> usize {
        self.reuse.iter().filter(|x| x.is_none()).count()
    }
    pub fn reused_count(&self) -> usize {
        self.reuse.iter().filter(|x| x.is_some()).count()
    }
}

/// Diff `old` vs `new` by key extracted with `id_of`.
pub fn diff_by_id<T, K, F>(old: &[T], new: &[T], id_of: F) -> DiffPlan
where
    K: Eq + Hash,
    F: Fn(&T) -> K,
{
    let mut old_pos: HashMap<K, usize> = HashMap::with_capacity(old.len());
    for (i, item) in old.iter().enumerate() {
        old_pos.entry(id_of(item)).or_insert(i);
    }

    let mut reuse = Vec::with_capacity(new.len());
    let mut seen_old = vec![false; old.len()];
    for item in new {
        let k = id_of(item);
        if let Some(&oi) = old_pos.get(&k) {
            if !seen_old[oi] {
                seen_old[oi] = true;
                reuse.push(Some(oi));
            } else {
                reuse.push(None);
            }
        } else {
            reuse.push(None);
        }
    }
    let mut removed_desc: Vec<usize> = seen_old
        .iter()
        .enumerate()
        .filter_map(|(i, seen)| if *seen { None } else { Some(i) })
        .collect();
    removed_desc.sort_unstable_by(|a, b| b.cmp(a));
    DiffPlan {
        reuse,
        removed_desc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Item(u32, &'static str);

    #[test]
    fn empty_to_items_all_inserted() {
        let plan = diff_by_id::<Item, _, _>(&[], &[Item(1, "a"), Item(2, "b")], |i| i.0);
        assert_eq!(plan.reuse, vec![None, None]);
        assert!(plan.removed_desc.is_empty());
    }

    #[test]
    fn identical_reuses_all() {
        let old = vec![Item(1, "a"), Item(2, "b")];
        let new = vec![Item(1, "a"), Item(2, "b")];
        let plan = diff_by_id(&old, &new, |i| i.0);
        assert_eq!(plan.reuse, vec![Some(0), Some(1)]);
        assert!(plan.removed_desc.is_empty());
    }

    #[test]
    fn remove_missing() {
        let old = vec![Item(1, "a"), Item(2, "b"), Item(3, "c")];
        let new = vec![Item(1, "a"), Item(3, "c")];
        let plan = diff_by_id(&old, &new, |i| i.0);
        assert_eq!(plan.reuse, vec![Some(0), Some(2)]);
        assert_eq!(plan.removed_desc, vec![1]);
    }

    #[test]
    fn insert_and_reorder() {
        let old = vec![Item(1, "a"), Item(2, "b")];
        let new = vec![Item(2, "b"), Item(3, "c"), Item(1, "a")];
        let plan = diff_by_id(&old, &new, |i| i.0);
        assert_eq!(plan.reuse, vec![Some(1), None, Some(0)]);
        assert!(plan.removed_desc.is_empty());
    }

    #[test]
    fn clear_all() {
        let old = vec![Item(1, "a")];
        let plan = diff_by_id::<Item, _, _>(&old, &[], |i| i.0);
        assert!(plan.reuse.is_empty());
        assert_eq!(plan.removed_desc, vec![0]);
    }
}
