const EWMA_ALPHA: f64 = 0.25;
const MIN_SPLIT_BYTES: u64 = 1024 * 1024;
const TARGET_PART_SECONDS: f64 = 15.0;

#[derive(Debug, Clone, Copy)]
pub(super) struct SplitInput {
    pub(super) part_id: u64,
    pub(super) cursor: u64,
    pub(super) end: u64,
    pub(super) ewma: Option<f64>,
    pub(super) active: bool,
    pub(super) has_stable_sample: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SplitPlan {
    pub(super) part_id: u64,
    pub(super) split_at: u64,
    pub(super) new_rate: f64,
}

pub(super) fn update_ewma(previous: Option<f64>, current: f64) -> f64 {
    previous.map_or(current, |old| {
        EWMA_ALPHA * current + (1.0 - EWMA_ALPHA) * old
    })
}

fn adaptive_min_bytes(rate: f64) -> u64 {
    ((rate * TARGET_PART_SECONDS).ceil() as u64).max(MIN_SPLIT_BYTES)
}

fn median_rate(mut rates: Vec<f64>) -> Option<f64> {
    if rates.is_empty() {
        return None;
    }
    rates.sort_by(f64::total_cmp);
    let middle = rates.len() / 2;
    Some(if rates.len().is_multiple_of(2) {
        (rates[middle - 1] + rates[middle]) / 2.0
    } else {
        rates[middle]
    })
}

pub(super) fn choose_split(
    parts: &[SplitInput],
    active_count: usize,
    max: usize,
) -> Option<SplitPlan> {
    if active_count >= max {
        return None;
    }
    let selected = parts
        .iter()
        .filter(|part| part.active && part.has_stable_sample && part.ewma.is_some())
        .filter(|part| part.cursor < part.end)
        .max_by_key(|part| part.end.saturating_sub(part.cursor))?;
    let selected_rate = selected.ewma?;
    let new_rate = median_rate(
        parts
            .iter()
            .filter(|part| {
                part.active && part.has_stable_sample && part.part_id != selected.part_id
            })
            .filter_map(|part| part.ewma)
            .collect(),
    )
    .unwrap_or(selected_rate);
    let cursor = selected.cursor;
    let remaining = selected.end - cursor;
    let selected_min = adaptive_min_bytes(selected_rate);
    let new_min = adaptive_min_bytes(new_rate);
    if remaining < selected_min.saturating_add(new_min) {
        return None;
    }
    let ideal_selected =
        ((remaining as f64) * selected_rate / (selected_rate + new_rate)).round() as u64;
    let selected_bytes = ideal_selected.clamp(selected_min, remaining - new_min);
    Some(SplitPlan {
        part_id: selected.part_id,
        split_at: cursor + selected_bytes,
        new_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewma_and_adaptive_minimum_follow_fixed_policy() {
        assert_eq!(update_ewma(None, 100.0), 100.0);
        assert_eq!(update_ewma(Some(100.0), 200.0), 125.0);
        assert_eq!(adaptive_min_bytes(1.0), 1024 * 1024);
        assert_eq!(adaptive_min_bytes(100_000.0), 1_500_000);
    }

    #[test]
    fn split_chooses_largest_sampled_active_interval_and_proportions_rates() {
        let parts = vec![
            split_input(0, 0, 10 * 1024 * 1024, 2 * 1024 * 1024, Some(2.0), true),
            split_input(1, 10 * 1024 * 1024, 16 * 1024 * 1024, 0, Some(1.0), true),
        ];
        let split = choose_split(&parts, 2, 3).unwrap();
        assert_eq!(split.part_id, 0);
        assert_eq!(split.new_rate, 1.0);
        assert_eq!(split.split_at, 7_689_557);
    }

    #[test]
    fn split_clamps_children_and_requires_sample_slot_and_enough_tail() {
        let no_sample = vec![split_input(0, 0, 4 * 1024 * 1024, 0, None, true)];
        assert!(choose_split(&no_sample, 1, 2).is_none());

        let sampled = vec![split_input(0, 0, 2 * 1024 * 1024 - 1, 0, Some(1.0), true)];
        assert!(choose_split(&sampled, 1, 2).is_none());
        assert!(choose_split(&sampled, 2, 2).is_none());

        let clamp = vec![
            split_input(0, 0, 5 * 1024 * 1024, 0, Some(100_000.0), true),
            split_input(1, 5 * 1024 * 1024, 6 * 1024 * 1024, 0, Some(1.0), true),
        ];
        let split = choose_split(&clamp, 1, 2).unwrap();
        assert_eq!(split.split_at, 4 * 1024 * 1024);
    }

    fn split_input(
        id: u64,
        start: u64,
        end: u64,
        downloaded: u64,
        ewma: Option<f64>,
        active: bool,
    ) -> SplitInput {
        SplitInput {
            part_id: id,
            cursor: start + downloaded,
            end,
            ewma,
            active,
            has_stable_sample: ewma.is_some(),
        }
    }
}
