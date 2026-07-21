use super::{PartSample, PartSampleEvent};
use crate::archive_download::http::archive_http_error;
use crate::archive_download::manifest::ManifestPart;
use crate::error::{Error, Result};
use futures_util::StreamExt;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, oneshot, watch};

const PART_WRITE_BUFFER_BYTES: usize = 2 * 1024 * 1024;

pub(super) enum AttemptOutcome {
    Complete,
    Incomplete(Error),
    Paused,
    RestartSequential(&'static str),
}

pub(super) struct AttemptTransfer<'a> {
    part: &'a ManifestPart,
    part_path: &'a Path,
    generation: u64,
    pause: &'a mut watch::Receiver<bool>,
    samples: &'a mpsc::UnboundedSender<PartSampleEvent>,
}

impl<'a> AttemptTransfer<'a> {
    pub(super) fn new(
        part: &'a ManifestPart,
        part_path: &'a Path,
        generation: u64,
        pause: &'a mut watch::Receiver<bool>,
        samples: &'a mpsc::UnboundedSender<PartSampleEvent>,
    ) -> Self {
        Self {
            part,
            part_path,
            generation,
            pause,
            samples,
        }
    }

    pub(super) async fn run(
        &mut self,
        response: reqwest::Response,
        downloaded: u64,
        request_started_at: Instant,
    ) -> Result<AttemptOutcome> {
        let file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(self.part_path)
            .await?;
        let mut writer = BufWriter::with_capacity(PART_WRITE_BUFFER_BYTES, file);
        let mut stream = response.bytes_stream();
        let mut logical_len = downloaded;
        let mut window_start_len = downloaded;
        let mut window_started_at = request_started_at;
        let mut transient_error = None;

        loop {
            let next = tokio::select! {
                biased;
                changed = self.pause.changed() => {
                    if changed.is_err() || *self.pause.borrow() {
                        return Ok(AttemptOutcome::Paused);
                    }
                    continue;
                }
                next = stream.next() => next,
            };

            match next {
                Some(Ok(chunk)) => {
                    let Some(next_len) = logical_len.checked_add(chunk.len() as u64) else {
                        return Ok(AttemptOutcome::RestartSequential(
                            "archive part response exceeds its interval",
                        ));
                    };
                    if next_len > self.part.len() {
                        return Ok(AttemptOutcome::RestartSequential(
                            "archive part response exceeds its interval",
                        ));
                    }
                    writer.write_all(&chunk).await?;
                    logical_len = next_len;

                    if window_started_at.elapsed() >= Duration::from_secs(1)
                        && logical_len > window_start_len
                    {
                        let durable_len = flush_and_emit_sample(
                            &mut writer,
                            self.part_path,
                            self.part.id,
                            self.generation,
                            window_start_len,
                            window_started_at,
                            self.samples,
                        )
                        .await?;
                        logical_len = durable_len;
                        window_start_len = durable_len;
                        window_started_at = Instant::now();
                    }
                }
                Some(Err(error)) => {
                    transient_error = Some(archive_http_error(error));
                    break;
                }
                None => break,
            }
        }

        let durable_len = flush_and_emit_sample(
            &mut writer,
            self.part_path,
            self.part.id,
            self.generation,
            window_start_len,
            window_started_at,
            self.samples,
        )
        .await?;
        if *self.pause.borrow() {
            return Ok(AttemptOutcome::Paused);
        }
        if durable_len == self.part.len() && transient_error.is_none() {
            return Ok(AttemptOutcome::Complete);
        }

        Ok(AttemptOutcome::Incomplete(transient_error.unwrap_or_else(
            || Error::Other("archive part response ended before its requested range".into()),
        )))
    }
}

pub(super) async fn part_file_len(path: &Path) -> Result<u64> {
    let metadata = tokio::fs::metadata(path).await?;
    if !metadata.is_file() {
        return Err(Error::Other("archive part is not a regular file".into()));
    }
    Ok(metadata.len())
}

async fn flush_and_emit_sample(
    writer: &mut BufWriter<tokio::fs::File>,
    part_path: &Path,
    part_id: u64,
    generation: u64,
    window_start_len: u64,
    window_started_at: Instant,
    samples: &mpsc::UnboundedSender<PartSampleEvent>,
) -> Result<u64> {
    writer.flush().await?;
    let durable_len = part_file_len(part_path).await?;
    let window_delta = durable_len.checked_sub(window_start_len).ok_or_else(|| {
        Error::Other("archive part durable length regressed while flushing".into())
    })?;
    let (applied, acknowledged) = oneshot::channel();
    samples
        .send(PartSampleEvent {
            sample: PartSample {
                part_id,
                generation,
                durable_len,
                window_delta,
                elapsed: window_started_at.elapsed(),
            },
            applied,
        })
        .map_err(|_| Error::Other("archive part sample receiver closed".into()))?;
    acknowledged
        .await
        .map_err(|_| Error::Other("archive part sample acknowledgement was dropped".into()))?;
    Ok(durable_len)
}
