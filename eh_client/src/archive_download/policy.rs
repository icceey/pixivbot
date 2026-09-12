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
