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

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use fnv::FnvHasher;

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

    // Phase 1: isolate size outliers as singletons
    let (mut result_groups, remaining_indices) = isolate_size_outliers(items, max_groups);

    // Phase 2: assign all remaining components using stability tiers and
    // name-based hashing
    if !remaining_indices.is_empty() {
        let remaining_budget = max_groups - result_groups.len();
        assert!(remaining_budget > 0, "no layers left for remaining items");
        let groups = bin_by_stability_tiers(items, &remaining_indices, remaining_budget);
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

/// Phase 1: Size-based statistical classification using median + MAD. Large
/// components get singleton layers regardless of their stability. Returns the
/// singleton groups and the indices of items that weren't isolated.
///
/// Singletons are capped at [`HIGH_SIZE_CAP`] of the budget. Excess items are
/// returned in the remaining pool, sorted so the largest stay as singletons.
fn isolate_size_outliers(items: &[PackItem], budget: usize) -> (Vec<PackGroup>, Vec<usize>) {
    let sizes: Vec<f64> = items.iter().map(|item| item.size as f64).collect();
    let median = compute_median(&sizes);
    let raw_mad = compute_mad(&sizes, median);

    // Corner-case: when MAD is near zero, the threshold collapses to the median
    // itself, which would classify roughly half the items as "outliers". This
    // happens when there are no or very few real outliers (since MAD is a
    // median, one big deviation among many zeros still yields MAD=0). In either
    // case, substitute the median as the effective MAD (effectively then the
    // threshold because 2.5 * median). This correctly avoids false positives
    // in the uniform case (all items go to phase 2), and as a happy accident
    // still catches genuinely large sparse outliers in an otherwise tight
    // distribution. This is in practice extremely unlikely because functioning
    // userspace rootfses are usually made of components with a very wide size
    // spread.
    let mad = if raw_mad < median * 0.01 {
        median
    } else {
        raw_mad
    };

    let high_size_limit = median + SIZE_OUTLIER_THRESHOLD * mad;

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

    // Cap high-size singletons at a fraction of the budget.
    // Excess are pushed into the remaining pool.
    let reserved_bins = if remaining_indices.is_empty() { 0 } else { 1 };
    let hs_bins_limit =
        ((budget.saturating_sub(reserved_bins) as f64) * HIGH_SIZE_CAP).floor() as usize;
    let hs_bins = high_size_indices.len().min(hs_bins_limit);
    if hs_bins < high_size_indices.len() {
        high_size_indices.sort_by(|&a, &b| items[b].size.cmp(&items[a].size));
        remaining_indices.extend_from_slice(&high_size_indices[hs_bins..]);
        high_size_indices.truncate(hs_bins);
        tracing::debug!(limit = hs_bins, "phase 1: capped high-size singletons");
    }

    let singletons = high_size_indices
        .iter()
        .map(|&idx| make_singleton(items, idx))
        .collect();
    (singletons, remaining_indices)
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
    let mut hasher = FnvHasher::default();
    name.hash(&mut hasher);
    hasher.finish()
}

/// Assigns non-singleton components to bins using stability-based tiers
/// and name-based hashing within each tier for deterministic assignment.
fn bin_by_stability_tiers(
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

    // Sub-classify by stability into three tiers using mean + stddev. NB: I
    // played with other strategies here, including using median+MAD or having a
    // different k value (than the implied 1 below) for the high and low limits.
    // The difference is minimal.
    let stabilities: Vec<f64> = indices.iter().map(|&i| items[i].stability).collect();
    let mean_stab = compute_mean(&stabilities);
    let stddev_stab = compute_stddev(&stabilities, mean_stab);

    let high_stab_limit = mean_stab + stddev_stab; // could spill above 1.0; that's fine, we just won't have a high tier
    let low_stab_limit = mean_stab - stddev_stab; // could spill below 0.0; that's fine, we just won't have a low tier

    tracing::debug!(
        mean_stab,
        stddev_stab,
        high_stab_limit,
        low_stab_limit,
        "phase 2: stability tier thresholds"
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

    // If we don't have enough bins for all tiers, skip tier separation
    // and hash everything together. With too few bins the tier distinction
    // isn't useful anyway.
    let tier_counts = [high_stab.len(), mid_stab.len(), low_stab.len()];
    let non_empty_count = tier_counts.iter().filter(|&&c| c > 0).count();
    if max_bins < non_empty_count {
        return hash_into_bins(items, indices, max_bins);
    }

    // Allocate bins proportionally by component count (at least 1 per non-empty tier)
    let bins_per_tier = allocate_tier_bins(&tier_counts, max_bins);

    tracing::debug!(?bins_per_tier, "phase 2: allocated tier bins");

    // Within each tier, assign to bins using name-based hashing
    let mut result = Vec::new();
    for (tier, num_bins) in [&high_stab, &mid_stab, &low_stab].iter().zip(bins_per_tier) {
        if num_bins > 0 {
            result.extend(hash_into_bins(items, tier, num_bins));
        } else {
            assert!(tier.is_empty(), "non-empty tier but no bin allocated");
        }
    }

    result
}

/// Allocates bins proportionally across tiers by component count. Each
/// non-empty tier gets at least 1 bin; the last non-empty tier gets the
/// remainder. Empty tiers get 0.
fn allocate_tier_bins(tier_counts: &[usize], max_bins: usize) -> Vec<usize> {
    let non_empty_count = tier_counts.iter().filter(|&&c| c > 0).count();
    let total: usize = tier_counts.iter().sum();

    let mut result = vec![0usize; tier_counts.len()];
    let mut allocated = 0;
    let mut non_empty_seen = 0;

    for (i, &count) in tier_counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        non_empty_seen += 1;
        if non_empty_seen == non_empty_count {
            // last non-empty tier we'll see; allocate everything remaining
            result[i] = max_bins.saturating_sub(allocated);
        } else {
            // Start with the proportional share, but at least 1 bin.
            let proportional = ((count as f64 / total as f64) * max_bins as f64)
                .round()
                .max(1.0) as usize;
            // Clamp so we reserve at least 1 bin per remaining non-empty tier.
            let remaining_tiers = non_empty_count - non_empty_seen;
            let max_allowed = max_bins.saturating_sub(allocated + remaining_tiers);
            result[i] = proportional.min(max_allowed);
            allocated += result[i];
        }
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

    // Hash+mod collisions mean we could end up with empty bins. Find those.
    let mut empty_bins: Vec<usize> = bins
        .iter()
        .enumerate()
        .filter(|(_, b)| b.is_empty())
        .map(|(i, _)| i)
        .collect();

    // Fill empty bins by moving the largest item (by size) out of any
    // multi-item bin. This isolates heavyweights so that if they change,
    // less data is invalidated.
    while let Some(empty_idx) = empty_bins.pop() {
        // Find the largest item across all bins that have more than 1 item.
        // (Yeah, this rescans everything for each empty bin, but meh... we're
        // dealing with ridiculously small numbers in computer terms.)
        let found = bins
            .iter()
            .enumerate()
            .filter(|(_, b)| b.len() > 1)
            .flat_map(|(bin_idx, b)| {
                b.iter()
                    .enumerate()
                    // This is confusing but basically:
                    // - bin_idx: the index of the item's bin in bins
                    // - bin_item_idx: the index of the item in its bin
                    // - item_idx: the index of the item in items
                    .map(move |(bin_item_idx, &item_idx)| (bin_idx, bin_item_idx, item_idx))
            })
            .max_by_key(|&(_, _, item_idx)| items[item_idx].size);

        if let Some((bin_idx, bin_item_idx, _)) = found {
            let moved = bins[bin_idx].swap_remove(bin_item_idx);
            bins[empty_idx].push(moved);
        } else {
            // no more bins with more than 1 item; stop handling empty bins
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
    if values.len() < 2 {
        return 0.0;
    }
    let variance = values.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
    variance.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
    fn test_trivial_cases() {
        // empty input
        assert!(calculate_packing(&[], 5).is_empty());

        let items = vec![make_item("a", 100, 0.5)];

        // max_groups = 0
        assert!(calculate_packing(&items, 0).is_empty());

        // single item
        let result = calculate_packing(&items, 5);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].indices, vec![0]);
        verify_packing_result(&items, &result, 5);

        // no packing needed
        let items = vec![
            make_item("a", 100, 0.9),
            make_item("b", 200, 0.8),
            make_item("c", 300, 0.7),
        ];
        let result = calculate_packing(&items, 3);
        assert_eq!(result.len(), 3);
        verify_packing_result(&items, &result, 3);
        let result = calculate_packing(&items, 4);
        assert_eq!(result.len(), 3);
        verify_packing_result(&items, &result, 4);

        // single group
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
    fn test_statistical_helpers() {
        // median
        assert_eq!(compute_median(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(compute_median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(compute_median(&[5.0]), 5.0);
        assert_eq!(compute_median(&[]), 0.0);

        // MAD
        assert_eq!(compute_mad(&[1.0, 2.0, 3.0], 2.0), 1.0);
    }

    #[test]
    fn test_fewer_bins_than_tiers() {
        // 1 huge outlier gets a singleton (also exercises the MAD=0 fallback
        // since the outlier doesn't move the median deviation), leaving 1 bin
        // for 9 items whose stabilities span 3 tiers. Previously this dropped
        // items from tiers that got 0 bins.
        let items = vec![
            make_item("huge", 100000, 0.5),
            make_item("a", 100, 0.99),
            make_item("b", 100, 0.98),
            make_item("c", 100, 0.97),
            make_item("d", 100, 0.70),
            make_item("e", 100, 0.65),
            make_item("f", 100, 0.60),
            make_item("g", 100, 0.10),
            make_item("h", 100, 0.05),
            make_item("i", 100, 0.01),
        ];
        let result = calculate_packing(&items, 3);
        verify_packing_result(&items, &result, 3);

        // the huge item must be in its own singleton group
        let huge_group = result.iter().find(|g| g.indices.contains(&0)).unwrap();
        assert_eq!(huge_group.indices.len(), 1);
    }
}
