//! # Packing Algorithm
//!
//! This module implements a clustering algorithm to merge a set of N
//! components into K groups while maximizing the group reuse across updates.
//! The general approach came out of a session with Gemini 3 Pro on an
//! abstracted version of the problem statement.
//!
//! ## Definitions
//!
//! 1. The "stability" of a component is the probability it doesn't change.
//! 2. The "size" of a component is the sum of all its files.
//! 3. The "expected value" of a component is stability x size; basically,
//!    long-term how much data do we avoid pulling from this component on
//!    average.
//! 4. The expected value of two components combined in the same group is
//!    (size1 + size2) x (stability1 x stability2).
//!
//! We want to find the group arrangement which maximize total expected value
//! (TEV). The final groups become OCI layers.
//!
//! ## Algorithm
//!
//! 1. Create a separate group for each component. If N <= K, we're done.
//! 2. Otherwise, we need to merge some groups. Every merge will reduce TEV.
//!    We should look for the merge that will result in the smallest TEV loss,
//!    so we first need to calculate the TEV loss for all possible merges. For
//!    every possible merge of two components, we calculate what the EV of the
//!    merged group would be and how much smaller it is from the EV of keeping
//!    them separate. Doing this for every component is O(N^2), but meh, N in
//!    our case is trivially small for modern computer speeds. Shove all those
//!    potential merges and their losses in a BinaryHeap. The top will then
//!    always be the merge with the smallest loss.
//! 3. Pop the heap to get the next optimal merge and do that merge. Calculate
//!    losses for this new merged group vs all the remaining groups and insert
//!    into the heap.
//! 4. Keep doing 3. until we get to K groups.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Input item for packing
#[derive(Debug, Clone)]
pub struct PackItem {
    /// Total size in bytes of all files in this component
    pub size: u64,
    /// Probability the component doesn't change between updates (0.0 to 1.0)
    pub stability: f64,
}

/// Output group from packing
#[derive(Debug, Clone)]
pub struct PackGroup {
    /// Indices into the original input slice
    pub indices: Vec<usize>,
    /// Total size in bytes of all files in this group
    pub size: u64,
    /// Combined stability of the group (product of individual stabilities)
    pub stability: f64,
}

impl PackGroup {
    fn expected_value(&self) -> f64 {
        self.size as f64 * self.stability
    }
}

/// A candidate merge operation stored in the heap.
#[derive(Debug)]
struct MergeCandidate {
    loss: f64,
    group_a_id: usize,
    group_b_id: usize,
}

impl Ord for MergeCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Note here we *reverse* the order of comparison because we want a
        // min-heap not a max-heap.
        other
            .loss
            .partial_cmp(&self.loss)
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for MergeCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for MergeCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.loss == other.loss
    }
}

impl Eq for MergeCandidate {}

/// Calculates how to pack items into at most `max_groups` groups in a way
/// that attempts to maximize group reuse. See module docstring for algorithm
/// details.
///
/// Returns groups sorted by stability descending (most stable first). Each
/// group contains indices into the original input slice.
pub fn calculate_packing(items: &[PackItem], max_groups: usize) -> Vec<PackGroup> {
    if items.is_empty() || max_groups == 0 {
        return Vec::new();
    }

    let n = items.len();
    tracing::debug!(components = n, max_layers = max_groups, "starting packing");

    // if we already have fewer items than max_groups, no packing is needed
    if n <= max_groups {
        tracing::debug!(components = n, "no packing needed, within max_layers");
        let mut result: Vec<PackGroup> = items
            .iter()
            .enumerate()
            .map(|(i, item)| PackGroup {
                indices: vec![i],
                size: item.size,
                stability: item.stability,
            })
            .collect();
        sort_by_stability_desc(&mut result);
        return result;
    }

    // use a Vec<Option> to track active groups; merged groups are appended
    let mut groups: Vec<Option<PackGroup>> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            Some(PackGroup {
                indices: vec![i],
                size: item.size,
                stability: item.stability,
            })
        })
        .collect();
    let mut active_count = n;
    let mut merge_candidates = BinaryHeap::new();

    // pre-calculate merge losses for all initial pairs
    for i in 0..n {
        for j in (i + 1)..n {
            // SAFETY: we just created these groups above
            let g_a = groups[i].as_ref().unwrap();
            let g_b = groups[j].as_ref().unwrap();

            let loss = calculate_merge_loss(g_a, g_b);
            merge_candidates.push(MergeCandidate {
                loss,
                group_a_id: i,
                group_b_id: j,
            });
        }
    }

    // do the next best merge until we're within the constraint
    let mut merge_count = 0usize;
    while active_count > max_groups {
        let Some(merge_op) = merge_candidates.pop() else {
            break;
        };

        // skip stale candidates (groups already merged)
        if groups[merge_op.group_a_id].is_none() || groups[merge_op.group_b_id].is_none() {
            continue;
        }

        // SAFETY: we just verified above that both are Some
        let g_a = groups[merge_op.group_a_id].take().unwrap();
        let g_b = groups[merge_op.group_b_id].take().unwrap();

        let mut new_indices = g_a.indices;
        new_indices.extend(g_b.indices);

        // append merged group
        let new_id = groups.len();
        let new_stability = g_a.stability * g_b.stability;
        tracing::trace!(
            merged_into = new_id,
            from_a = merge_op.group_a_id,
            from_b = merge_op.group_b_id,
            loss = merge_op.loss,
            new_stability = new_stability,
            "merged groups"
        );
        groups.push(Some(PackGroup {
            indices: new_indices,
            size: g_a.size + g_b.size,
            stability: new_stability,
        }));
        active_count -= 1;
        merge_count += 1;

        // calculate losses between new group and all remaining groups
        let created_group = groups[new_id].as_ref().unwrap();
        for (other_id, other_group_opt) in groups.iter().enumerate() {
            if other_id == new_id {
                continue;
            }
            // is this still an active group?
            if let Some(other_group) = other_group_opt {
                let loss = calculate_merge_loss(created_group, other_group);
                merge_candidates.push(MergeCandidate {
                    loss,
                    group_a_id: new_id,
                    group_b_id: other_id,
                });
            }
        }
    }
    tracing::debug!(merges = merge_count, "packing merges performed");

    // collect and sort results by stability descending
    let mut result: Vec<PackGroup> = groups.into_iter().flatten().collect();
    sort_by_stability_desc(&mut result);
    result
}

fn sort_by_stability_desc(items: &mut [PackGroup]) {
    items.sort_by(|a, b| {
        b.stability
            .partial_cmp(&a.stability)
            .unwrap_or(Ordering::Equal)
    });
}

fn calculate_merge_loss(a: &PackGroup, b: &PackGroup) -> f64 {
    let ev_separate = a.expected_value() + b.expected_value();

    let combined_size = (a.size + b.size) as f64;
    let combined_prob = a.stability * b.stability;
    let ev_merged = combined_size * combined_prob;

    // Loss = expected value destroyed by merging
    ev_separate - ev_merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // Note from author: it's tricky to test this algorithm properly because
    // since it's a greedy algorithm, it's not guaranteed to always yield
    // the truly optimal solution. Here we test some simplified cases. In the
    // future, it'd be nice to set up a harness with real test data that we can
    // use to evaluate different algorithms or potential improvements. At least
    // that way we get a comparative validation of the algorithm.

    /// Verifies invariants that must hold for any valid packing result.
    fn verify_packing_result(input: &[PackItem], result: &[PackGroup], max_groups: usize) {
        // check group count respects max_groups
        assert!(
            result.len() <= max_groups,
            "too many groups: {} > {}",
            result.len(),
            max_groups
        );

        // check all indices present exactly once (no loss, no duplication)
        let mut output_indices: Vec<usize> =
            result.iter().flat_map(|g| &g.indices).copied().collect();
        output_indices.sort();
        let expected_indices: Vec<usize> = (0..input.len()).collect();
        assert_eq!(output_indices, expected_indices, "indices mismatch");

        // check no empty groups
        assert!(
            result.iter().all(|g| !g.indices.is_empty()),
            "found empty group"
        );

        // check sorted by stability descending
        for i in 1..result.len() {
            assert!(
                result[i - 1].stability >= result[i].stability,
                "groups not sorted by stability: {:?}",
                result.iter().map(|g| g.stability).collect::<Vec<_>>()
            );
        }

        // check total size preserved
        let input_total: u64 = input.iter().map(|c| c.size).sum();
        let output_total: u64 = result
            .iter()
            .flat_map(|g| &g.indices)
            .map(|&idx| input[idx].size)
            .sum();
        assert_eq!(input_total, output_total, "total size mismatch");
    }

    #[test]
    fn test_edge_cases() {
        // empty input
        assert!(calculate_packing(&[], 5).is_empty());

        // max_groups = 0
        let items = vec![PackItem {
            size: 100,
            stability: 0.5,
        }];
        assert!(calculate_packing(&items, 0).is_empty());

        // single item
        let items = vec![PackItem {
            size: 100,
            stability: 0.5,
        }];
        let result = calculate_packing(&items, 5);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].indices, vec![0]);
        verify_packing_result(&items, &result, 5);
    }

    #[test]
    fn test_no_packing_needed() {
        // Items with different stabilities
        let items = vec![
            PackItem {
                size: 100,
                stability: 0.9,
            },
            PackItem {
                size: 200,
                stability: 0.8,
            },
            PackItem {
                size: 300,
                stability: 0.7,
            },
        ];
        let result = calculate_packing(&items, 5);
        assert_eq!(result.len(), 3);
        // Should be sorted by stability descending
        assert_eq!(result[0].indices, vec![0]); // 0.9
        assert_eq!(result[1].indices, vec![1]); // 0.8
        assert_eq!(result[2].indices, vec![2]); // 0.7
        verify_packing_result(&items, &result, 5);
    }

    #[test]
    fn test_pack_to_one_group() {
        let items = vec![
            PackItem {
                size: 100,
                stability: 0.5,
            },
            PackItem {
                size: 200,
                stability: 0.5,
            },
            PackItem {
                size: 300,
                stability: 0.5,
            },
        ];
        let result = calculate_packing(&items, 1);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].indices.len(), 3);
        // All items should be in the single group
        let indices: HashSet<usize> = result[0].indices.iter().copied().collect();
        assert_eq!(indices, HashSet::from([0, 1, 2]));
        verify_packing_result(&items, &result, 1);
    }

    #[test]
    fn test_size_constant_stability_changes() {
        // merging two high-stability items has less loss than merging
        // a high-stability with a low-stability item
        // index 0: stable_1, index 1: stable_2, index 2: unstable
        let items = vec![
            PackItem {
                size: 1000,
                stability: 0.99,
            },
            PackItem {
                size: 1000,
                stability: 0.99,
            },
            PackItem {
                size: 1000,
                stability: 0.3,
            },
        ];
        let result = calculate_packing(&items, 2);
        assert_eq!(result.len(), 2);

        // the two stable items (indices 0 and 1) should be merged together
        let merged_group = result
            .iter()
            .find(|g| g.indices.len() == 2)
            .expect("one group should have 2 items");
        let merged_indices: HashSet<usize> = merged_group.indices.iter().copied().collect();
        assert_eq!(merged_indices, HashSet::from([0, 1]));
        verify_packing_result(&items, &result, 2);
    }

    #[test]
    fn test_stability_constant_size_changes() {
        // with uniform stability, merging smaller items has less loss
        // index 0: huge, index 1: small1, index 2: small2
        let items = vec![
            PackItem {
                size: 10000,
                stability: 0.5,
            },
            PackItem {
                size: 10,
                stability: 0.5,
            },
            PackItem {
                size: 10,
                stability: 0.5,
            },
        ];
        let result = calculate_packing(&items, 2);
        assert_eq!(result.len(), 2);

        // The two small items (indices 1 and 2) should be merged together (least loss)
        // The huge item (index 0) should stay separate
        let huge_group = result.iter().find(|g| g.indices.contains(&0));
        assert!(huge_group.is_some());
        assert_eq!(huge_group.unwrap().indices.len(), 1);

        let small_group = result.iter().find(|g| g.indices.contains(&1));
        assert!(small_group.is_some());
        assert!(small_group.unwrap().indices.contains(&2));
        verify_packing_result(&items, &result, 2);
    }
}
