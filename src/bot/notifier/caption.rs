use super::ContinuationNumbering;

/// 文案策略：区分“共享文案”和“独立文案”
pub(super) enum CaptionStrategy<'a> {
    /// 所有图片共享一个 Caption (仅第一张显示)
    Shared(Option<&'a str>),
    /// 每张图片有独立的 Caption
    Individual(&'a [String]),
}

pub(super) fn shared_batch_caption(
    base_caption: Option<&str>,
    item_idx: usize,
    batch_idx: usize,
    continuation_numbering: ContinuationNumbering,
) -> Option<String> {
    if item_idx != 0 {
        return None;
    }

    if batch_idx == 0 && continuation_numbering.first_batch_number == 1 {
        return base_caption.map(str::to_owned);
    }

    (continuation_numbering.total_batches > 1).then(|| {
        format!(
            "\\(continued {}/{}\\)",
            continuation_numbering.display_batch_number(batch_idx),
            continuation_numbering.total_batches
        )
    })
}

pub(super) fn individual_batch_caption(
    raw_caption: &str,
    item_idx: usize,
    batch_idx: usize,
    continuation_numbering: ContinuationNumbering,
) -> Option<String> {
    if item_idx == 0 && (batch_idx > 0 || continuation_numbering.first_batch_number > 1) {
        Some(format!(
            "\\(continued {}/{}\\)\n\n{}",
            continuation_numbering.display_batch_number(batch_idx),
            continuation_numbering.total_batches,
            raw_caption
        ))
    } else {
        Some(raw_caption.to_owned())
    }
}
