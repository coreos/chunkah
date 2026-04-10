//! # Packing Algorithm
//!
//! This module implements a statistical partitioning algorithm to assign N
//! components into at most K groups (OCI layers) while maximizing layer reuse
//! across updates. The approach is inspired by rpm-ostree's container
//! encapsulation chunking algorithm.
//!
//! ## Definitions
//!
//! 1. The "stability" of a component is the probability it doesn't change.
//! 2. The "size" of a component is the sum of all its files.
//!
//! ## Algorithm
//!
//! The algorithm has two phases:
//!
//! ### Phase 1: Size-Based Statistical Classification
//!
//! All components are classified using median and median absolute deviation
//! (MAD) of their sizes:
//! - **High-size outliers** (size >= median + threshold*MAD): Each gets its
//!   own singleton layer, regardless of stability. This protects both stable
//!   heavyweights (linux-firmware) and volatile ones (firefox, kernel).
//!   Capped at 80% of the budget; excess go to the remaining pool.
//! - Everything else goes to Phase 2.
//!
//! ### Phase 2: Stability-Tiered Hash Binning
//!
//! All remaining components are classified by stability into three tiers
//! (high, medium, low) using mean and standard deviation. Each tier gets
//! a proportional share of the remaining layer budget. Within each tier,
//! components are assigned to bins deterministically using a hash of
//! their component name. This ensures stable bin membership across
//! builds without needing to track prior build state.
//!

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// MAD multiplier for size outlier detection.
const SIZE_OUTLIER_THRESHOLD: f64 = 1.5;

/// Max fraction of layer budget for high-size singletons.
const HIGH_SIZE_CAP: f64 = 0.8;

/// Input item for packing
#[derive(Debug, Clone)]
pub struct PackItem {
    /// Component name, used for deterministic bin assignment
    pub name: String,
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

    let size_outlier_threshold = SIZE_OUTLIER_THRESHOLD;
    let high_size_cap = HIGH_SIZE_CAP;

    let mut result_groups: Vec<PackGroup> = Vec::new();
    let mut remaining_budget = max_groups;

    // Phase 1: Size-based statistical classification using median + MAD on
    // ALL items. Large components get singleton layers regardless of their
    // stability -- this protects both stable heavyweights (linux-firmware)
    // and volatile heavyweights (firefox, kernel) from contaminating other
    // layers.
    //
    // When MAD is near zero (all sizes similar), use the median itself as the
    // effective MAD to avoid classifying everything as an outlier.
    let sizes: Vec<f64> = (0..n).map(|i| items[i].size as f64).collect();
    let median = compute_median(&sizes);
    let raw_mad = compute_mad(&sizes, median);
    let mad = if raw_mad < median * 0.01 {
        median
    } else {
        raw_mad
    };

    let high_size_limit = median + size_outlier_threshold * mad;

    tracing::debug!(median, mad, high_size_limit, "phase 1: size thresholds");

    let mut high_size_indices: Vec<usize> = Vec::new();
    let mut remaining_indices: Vec<usize> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        if item.size as f64 >= high_size_limit {
            high_size_indices.push(i);
        } else {
            remaining_indices.push(i);
        }
    }

    tracing::debug!(
        high_size = high_size_indices.len(),
        remaining = remaining_indices.len(),
        "phase 1: size classification"
    );

    // Cap high-size singletons at a fraction of the budget (default 80%).
    // Excess are pushed into the remaining pool.
    let reserved_bins = if remaining_indices.is_empty() { 0 } else { 1 };
    let hs_limit =
        ((remaining_budget.saturating_sub(reserved_bins) as f64) * high_size_cap).floor() as usize;
    let hs_bins = high_size_indices.len().min(hs_limit);
    if hs_bins < high_size_indices.len() {
        sort_indices_by_size_desc(items, &mut high_size_indices);
        remaining_indices.extend_from_slice(&high_size_indices[hs_bins..]);
        high_size_indices.truncate(hs_bins);
        tracing::debug!(hs_bins, "phase 1: capped high-size singletons");
    }

    for &idx in &high_size_indices {
        result_groups.push(make_singleton(items, idx));
    }
    remaining_budget -= hs_bins;

    // Phase 2: Assign all remaining components using stability tiers and
    // name-based hashing. Components are split into stability tiers
    // (high/mid/low using mean+stddev) and each tier gets bins proportional
    // to its item count. Within each tier, components are assigned to bins
    // deterministically using a hash of their name.
    if !remaining_indices.is_empty() && remaining_budget > 0 {
        let groups = assign_remaining_components(items, &remaining_indices, remaining_budget);
        result_groups.extend(groups);
    }

    sort_by_stability_desc(&mut result_groups);
    result_groups
}

fn sort_by_stability_desc(groups: &mut [PackGroup]) {
    groups.sort_by(|a, b| {
        b.stability
            .partial_cmp(&a.stability)
            .unwrap_or(Ordering::Equal)
    });
}

fn sort_indices_by_size_desc(items: &[PackItem], indices: &mut [usize]) {
    indices.sort_by(|&a, &b| items[b].size.cmp(&items[a].size));
}

fn make_singleton(items: &[PackItem], idx: usize) -> PackGroup {
    PackGroup {
        indices: vec![idx],
        size: items[idx].size,
        stability: items[idx].stability,
    }
}

fn make_group(items: &[PackItem], indices: Vec<usize>) -> PackGroup {
    let total_size: u64 = indices.iter().map(|&i| items[i].size).sum();
    let combined_stability: f64 = indices.iter().map(|&i| items[i].stability).product();
    PackGroup {
        indices,
        size: total_size,
        stability: combined_stability,
    }
}

fn hash_name(name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// Assigns non-singleton components to bins using stability-based tiers
/// and name-based hashing within each tier for deterministic assignment.
fn assign_remaining_components(
    items: &[PackItem],
    indices: &[usize],
    max_bins: usize,
) -> Vec<PackGroup> {
    if indices.is_empty() || max_bins == 0 {
        return Vec::new();
    }

    // If everything fits, no grouping needed
    if indices.len() <= max_bins {
        return indices.iter().map(|&i| make_singleton(items, i)).collect();
    }

    // Sub-classify by stability into three tiers using mean + stddev
    let stabilities: Vec<f64> = indices.iter().map(|&i| items[i].stability).collect();
    let mean_stab = compute_mean(&stabilities);
    let stddev_stab = compute_stddev(&stabilities, mean_stab);

    let high_stab_limit = mean_stab + stddev_stab;
    let low_stab_limit = (mean_stab - stddev_stab).max(0.0);

    tracing::debug!(
        mean_stab,
        stddev_stab,
        high_stab_limit,
        low_stab_limit,
        "phase 3: stability tier thresholds"
    );

    let mut high_stab: Vec<usize> = Vec::new();
    let mut mid_stab: Vec<usize> = Vec::new();
    let mut low_stab: Vec<usize> = Vec::new();

    for &idx in indices {
        if items[idx].stability >= high_stab_limit {
            high_stab.push(idx);
        } else if items[idx].stability <= low_stab_limit {
            low_stab.push(idx);
        } else {
            mid_stab.push(idx);
        }
    }

    tracing::debug!(
        high_stab = high_stab.len(),
        mid_stab = mid_stab.len(),
        low_stab = low_stab.len(),
        "phase 3: stability tiers"
    );

    // Allocate bins proportionally by component count (at least 1 per non-empty tier)
    let tiers: Vec<&[usize]> = [
        high_stab.as_slice(),
        mid_stab.as_slice(),
        low_stab.as_slice(),
    ]
    .into_iter()
    .filter(|t| !t.is_empty())
    .collect();

    let non_empty_tiers = tiers.len();
    let total_components: usize = tiers.iter().map(|t| t.len()).sum();

    let mut tier_bins: Vec<usize> = Vec::with_capacity(non_empty_tiers);
    let mut allocated = 0;
    for (i, tier) in tiers.iter().enumerate() {
        if i == non_empty_tiers - 1 {
            // last tier gets the remainder
            tier_bins.push(max_bins.saturating_sub(allocated));
        } else {
            let bins = ((tier.len() as f64 / total_components as f64) * max_bins as f64)
                .round()
                .max(1.0) as usize;
            let remaining_for_others = non_empty_tiers - i - 1;
            let bins = bins.min(max_bins.saturating_sub(allocated + remaining_for_others));
            tier_bins.push(bins);
            allocated += bins;
        }
    }

    tracing::debug!(tier_bins = ?tier_bins, "phase 3: bins per tier");

    // Within each tier, assign to bins using name-based hashing
    let mut result = Vec::new();
    for (tier_indices, &num_bins) in tiers.iter().zip(tier_bins.iter()) {
        let groups = hash_into_bins(items, tier_indices, num_bins);
        result.extend(groups);
    }

    result
}

/// Distributes components into bins using a hash of the component name.
fn hash_into_bins(items: &[PackItem], indices: &[usize], num_bins: usize) -> Vec<PackGroup> {
    if indices.is_empty() || num_bins == 0 {
        return Vec::new();
    }

    let mut bins: Vec<Vec<usize>> = vec![Vec::new(); num_bins];

    for &idx in indices {
        let hash = hash_name(&items[idx].name);
        let bin = (hash as usize) % num_bins;
        bins[bin].push(idx);
    }

    // Handle empty bins by redistributing from the largest bin
    let mut empty_bins: Vec<usize> = bins
        .iter()
        .enumerate()
        .filter(|(_, b)| b.is_empty())
        .map(|(i, _)| i)
        .collect();

    while let Some(empty_idx) = empty_bins.pop() {
        // Find the largest bin with more than 1 item
        let largest = bins
            .iter()
            .enumerate()
            .filter(|(_, b)| b.len() > 1)
            .max_by_key(|(_, b)| b.len())
            .map(|(i, _)| i);

        if let Some(largest_idx) = largest {
            if let Some(moved) = bins[largest_idx].pop() {
                bins[empty_idx].push(moved);
            }
        } else {
            break;
        }
    }

    bins.into_iter()
        .filter(|b| !b.is_empty())
        .map(|b| make_group(items, b))
        .collect()
}

fn compute_median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

fn compute_mad(values: &[f64], median: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let deviations: Vec<f64> = values.iter().map(|&v| (v - median).abs()).collect();
    compute_median(&deviations)
}

fn compute_mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn compute_stddev(values: &[f64], mean: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let variance = values.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    variance.sqrt()
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

    fn make_item(name: &str, size: u64, stability: f64) -> PackItem {
        PackItem {
            name: name.to_string(),
            size,
            stability,
        }
    }

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
        let items = vec![make_item("a", 100, 0.5)];
        assert!(calculate_packing(&items, 0).is_empty());

        // single item
        let items = vec![make_item("a", 100, 0.5)];
        let result = calculate_packing(&items, 5);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].indices, vec![0]);
        verify_packing_result(&items, &result, 5);
    }

    #[test]
    fn test_no_packing_needed() {
        // Items with different stabilities
        let items = vec![
            make_item("a", 100, 0.9),
            make_item("b", 200, 0.8),
            make_item("c", 300, 0.7),
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
            make_item("a", 100, 0.5),
            make_item("b", 200, 0.5),
            make_item("c", 300, 0.5),
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
    fn test_large_components_get_singletons() {
        // One very large component and several small ones
        let items = vec![
            make_item("huge", 100000, 0.9),
            make_item("small1", 10, 0.9),
            make_item("small2", 10, 0.9),
            make_item("small3", 10, 0.9),
            make_item("small4", 10, 0.9),
        ];
        let result = calculate_packing(&items, 3);

        // The huge item should be in its own group
        let huge_group = result.iter().find(|g| g.indices.contains(&0));
        assert!(huge_group.is_some());
        assert_eq!(huge_group.unwrap().indices.len(), 1);
        verify_packing_result(&items, &result, 3);
    }

    #[test]
    fn test_volatile_components_get_singletons() {
        // One volatile component and two stable ones
        let items = vec![
            make_item("stable1", 1000, 0.99),
            make_item("stable2", 1000, 0.99),
            make_item("volatile", 1000, 0.1),
        ];
        let result = calculate_packing(&items, 2);

        // The volatile item should be isolated (not merged with stable ones)
        let volatile_group = result.iter().find(|g| g.indices.contains(&2));
        assert!(volatile_group.is_some());
        assert_eq!(volatile_group.unwrap().indices.len(), 1);
        verify_packing_result(&items, &result, 2);
    }

    #[test]
    fn test_deterministic_binning() {
        // Same inputs should always produce the same output
        let items: Vec<PackItem> = (0..20)
            .map(|i| make_item(&format!("pkg-{i}"), 1000 + i * 100, 0.8))
            .collect();
        let result1 = calculate_packing(&items, 5);
        let result2 = calculate_packing(&items, 5);

        assert_eq!(result1.len(), result2.len());
        for (g1, g2) in result1.iter().zip(result2.iter()) {
            let mut i1 = g1.indices.clone();
            let mut i2 = g2.indices.clone();
            i1.sort();
            i2.sort();
            assert_eq!(i1, i2, "non-deterministic binning");
        }
    }

    #[test]
    fn test_statistical_helpers() {
        // median
        assert_eq!(compute_median(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(compute_median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(compute_median(&[5.0]), 5.0);
        assert_eq!(compute_median(&[]), 0.0);

        // MAD
        assert_eq!(compute_mad(&[1.0, 2.0, 3.0], 2.0), 1.0);

        // mean
        assert_eq!(compute_mean(&[1.0, 2.0, 3.0]), 2.0);

        // stddev
        let sd = compute_stddev(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0], 5.0);
        assert!((sd - 2.0).abs() < 0.01);
    }
}
