use crate::archive_download::manifest::ManifestPart;
use crate::archive_download::part::PartSample;
use crate::archive_download::policy::{update_ewma, SplitInput};
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RuntimePart {
    pub(super) part: ManifestPart,
    pub(super) downloaded: u64,
    pub(super) ewma: Option<f64>,
    pub(super) attempts_used: usize,
    pub(super) active: bool,
    pub(super) has_stable_sample: bool,
    pub(super) generation: u64,
}

impl RuntimePart {
    pub(super) fn split_input(&self) -> SplitInput {
        SplitInput {
            part_id: self.part.id,
            cursor: self.part.start + self.downloaded,
            end: self.part.end,
            ewma: self.ewma,
            active: self.active,
            has_stable_sample: self.has_stable_sample,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SampleDisposition {
    Ignored,
    Reconciled,
    Stable,
}

pub(super) fn apply_part_sample(
    runtime: &mut RuntimePart,
    sample: PartSample,
) -> Result<SampleDisposition> {
    if sample.part_id != runtime.part.id
        || !runtime.active
        || sample.generation != runtime.generation
    {
        return Ok(SampleDisposition::Ignored);
    }
    if sample.durable_len > runtime.part.len() {
        return Err(Error::Other(
            "archive part sample exceeds its interval".into(),
        ));
    }
    if sample.durable_len < runtime.downloaded {
        return Err(Error::Other(
            "archive part sample durable length regressed".into(),
        ));
    }
    let expected = runtime
        .downloaded
        .checked_add(sample.window_delta)
        .ok_or_else(|| Error::Other("archive part sample byte count overflows u64".into()))?;
    if expected != sample.durable_len {
        return Err(Error::Other(
            "archive part sample delta does not match its durable length".into(),
        ));
    }

    let eligible = sample.is_rate_eligible();
    let next_ewma = eligible.then(|| {
        update_ewma(
            runtime.ewma,
            sample.window_delta as f64 / sample.elapsed.as_secs_f64(),
        )
    });
    runtime.downloaded = sample.durable_len;
    if let Some(ewma) = next_ewma {
        runtime.ewma = Some(ewma);
        runtime.has_stable_sample = true;
        Ok(SampleDisposition::Stable)
    } else {
        Ok(SampleDisposition::Reconciled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sample_reconciliation_is_exact_once_and_updates_only_stable_ewma() {
        let mut runtime = runtime_part(ManifestPart {
            id: 4,
            start: 100,
            end: 500,
        });
        runtime.downloaded = 25;
        runtime.ewma = Some(100.0);
        runtime.generation = 7;

        let short = PartSample {
            part_id: 4,
            generation: 7,
            durable_len: 89,
            window_delta: 64,
            elapsed: Duration::from_millis(500),
        };
        assert_eq!(
            apply_part_sample(&mut runtime, short).unwrap(),
            SampleDisposition::Reconciled
        );
        assert_eq!(runtime.downloaded, 89);
        assert_eq!(runtime.split_input().cursor, 189);
        assert_eq!(runtime.ewma, Some(100.0));
        assert!(!runtime.has_stable_sample);

        let reconciled = runtime.clone();
        assert!(apply_part_sample(&mut runtime, short).is_err());
        assert_eq!(runtime, reconciled);

        assert_eq!(
            apply_part_sample(
                &mut runtime,
                PartSample {
                    durable_len: 153,
                    elapsed: Duration::from_secs(1),
                    ..short
                },
            )
            .unwrap(),
            SampleDisposition::Stable
        );
        assert_eq!(runtime.downloaded, 153);
        assert_eq!(runtime.split_input().cursor, 253);
        assert_eq!(runtime.ewma, Some(91.0));
        assert!(runtime.has_stable_sample);
    }

    #[test]
    fn stale_samples_are_ignored_without_mutation() {
        let mut runtime = runtime_part(ManifestPart {
            id: 4,
            start: 100,
            end: 500,
        });
        runtime.downloaded = 25;
        runtime.ewma = Some(100.0);
        runtime.generation = 7;
        let before = runtime.clone();

        let disposition = apply_part_sample(
            &mut runtime,
            PartSample {
                part_id: 4,
                generation: 6,
                durable_len: 89,
                window_delta: 64,
                elapsed: Duration::from_secs(1),
            },
        )
        .unwrap();

        assert_eq!(disposition, SampleDisposition::Ignored);
        assert_eq!(runtime, before);

        for mut ignored_runtime in [
            {
                let mut inactive = before.clone();
                inactive.active = false;
                inactive
            },
            before.clone(),
        ] {
            let snapshot = ignored_runtime.clone();
            let sample = PartSample {
                part_id: if ignored_runtime.active { 99 } else { 4 },
                generation: 7,
                durable_len: 89,
                window_delta: 64,
                elapsed: Duration::from_secs(1),
            };
            assert_eq!(
                apply_part_sample(&mut ignored_runtime, sample).unwrap(),
                SampleDisposition::Ignored
            );
            assert_eq!(ignored_runtime, snapshot);
        }
    }

    #[test]
    fn invalid_samples_are_rejected_without_mutation() {
        let base = {
            let mut runtime = runtime_part(ManifestPart {
                id: 4,
                start: 100,
                end: 500,
            });
            runtime.downloaded = 25;
            runtime.generation = 7;
            runtime
        };
        for sample in [
            PartSample {
                part_id: 4,
                generation: 7,
                durable_len: 401,
                window_delta: 376,
                elapsed: Duration::from_secs(1),
            },
            PartSample {
                part_id: 4,
                generation: 7,
                durable_len: 25,
                window_delta: u64::MAX,
                elapsed: Duration::from_secs(1),
            },
            PartSample {
                part_id: 4,
                generation: 7,
                durable_len: 24,
                window_delta: 0,
                elapsed: Duration::from_secs(1),
            },
            PartSample {
                part_id: 4,
                generation: 7,
                durable_len: 31,
                window_delta: 5,
                elapsed: Duration::from_secs(1),
            },
        ] {
            let mut runtime = base.clone();
            assert!(apply_part_sample(&mut runtime, sample).is_err());
            assert_eq!(runtime, base);
        }
    }

    fn runtime_part(part: ManifestPart) -> RuntimePart {
        RuntimePart {
            part,
            downloaded: 0,
            ewma: None,
            attempts_used: 0,
            active: true,
            has_stable_sample: false,
            generation: 0,
        }
    }
}
