use super::attempt::{part_file_len, AttemptOutcome, AttemptTransfer};
use super::{
    part_get, requested_range, validate_part_response, PartExit, PartFailure, PartFailureKind,
    PartSampleEvent, SeedResponse, Validator,
};
use crate::archive_download::manifest::ManifestPart;
use crate::error::Error;
use crate::models::EhCookies;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

const PART_MAX_ATTEMPTS: usize = 4;

#[allow(clippy::too_many_arguments)]
pub(in crate::archive_download) async fn run_part(
    http: reqwest::Client,
    cookies: EhCookies,
    url: String,
    total_len: u64,
    validator: Validator,
    part: ManifestPart,
    part_path: PathBuf,
    mut attempts_used: usize,
    generation: u64,
    mut seed: Option<SeedResponse>,
    mut pause: watch::Receiver<bool>,
    samples: mpsc::UnboundedSender<PartSampleEvent>,
) -> (u64, PartExit) {
    loop {
        if *pause.borrow() {
            return (part.id, PartExit::Paused { attempts_used });
        }

        let downloaded = match part_file_len(&part_path).await {
            Ok(downloaded) => downloaded,
            Err(error) => return (part.id, failed(error, attempts_used)),
        };
        let range = match requested_range(&part, downloaded) {
            Ok(Some(range)) => range,
            Ok(None) => return (part.id, PartExit::Complete { attempts_used }),
            Err(failure) => return (part.id, PartExit::Failed(failure)),
        };
        if attempts_used >= PART_MAX_ATTEMPTS {
            return (
                part.id,
                failed(
                    Error::Other("archive part exhausted retry attempts".into()),
                    attempts_used,
                ),
            );
        }

        let (response, request_started_at) = if let Some(seed) = seed.take() {
            attempts_used += 1;
            (seed.response, seed.request_started_at)
        } else {
            let request_started_at = Instant::now();
            attempts_used += 1;
            let request = part_get(&http, &cookies, &url, range.0, range.1, &validator);
            let response = tokio::select! {
                biased;
                changed = pause.changed() => {
                    if changed.is_err() || *pause.borrow() {
                        attempts_used -= 1;
                        return (part.id, PartExit::Paused { attempts_used });
                    }
                    continue;
                }
                response = request.send() => response,
            };
            match response {
                Ok(response) => (response, request_started_at),
                Err(error) => {
                    let failure = PartFailure::retryable_http(error, attempts_used);
                    if attempts_used >= PART_MAX_ATTEMPTS {
                        return (part.id, PartExit::Failed(failure));
                    }
                    if wait_before_retry(&mut pause).await {
                        return (part.id, PartExit::Paused { attempts_used });
                    }
                    continue;
                }
            }
        };

        if let Err(mut failure) = validate_part_response(
            response.status(),
            response.headers(),
            range.0,
            range.1,
            total_len,
            &validator,
        ) {
            failure.attempts = attempts_used;
            return (part.id, PartExit::Failed(failure));
        }

        let outcome =
            match AttemptTransfer::new(&part, &part_path, generation, &mut pause, &samples)
                .run(response, downloaded, request_started_at)
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => return (part.id, failed(error, attempts_used)),
            };
        let failure = match outcome {
            AttemptOutcome::Complete => {
                return (part.id, PartExit::Complete { attempts_used });
            }
            AttemptOutcome::Paused => {
                attempts_used -= 1;
                return (part.id, PartExit::Paused { attempts_used });
            }
            AttemptOutcome::RestartSequential(message) => {
                return (part.id, restart_failure(message, attempts_used));
            }
            AttemptOutcome::Incomplete(error) => error,
        };

        if attempts_used >= PART_MAX_ATTEMPTS {
            return (part.id, failed(failure, attempts_used));
        }
        if wait_before_retry(&mut pause).await {
            return (part.id, PartExit::Paused { attempts_used });
        }
    }
}

async fn wait_before_retry(pause: &mut watch::Receiver<bool>) -> bool {
    if *pause.borrow() {
        return true;
    }
    tokio::select! {
        biased;
        changed = pause.changed() => changed.is_err() || *pause.borrow(),
        () = tokio::time::sleep(Duration::from_secs(1)) => false,
    }
}

fn failed(error: Error, attempts: usize) -> PartExit {
    PartExit::Failed(PartFailure {
        kind: PartFailureKind::Retryable,
        error,
        attempts,
    })
}

fn restart_failure(message: &'static str, attempts: usize) -> PartExit {
    let mut failure = PartFailure::restart_sequential(message);
    failure.attempts = attempts;
    PartExit::Failed(failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_download::manifest::ArchiveManifest;
    use crate::ArchiveArtifacts;
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn part_worker_emits_durable_reconciliation_before_retry_after_transient_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("content-range", "bytes 0-7/8")
                    .set_body_bytes(b"abc"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("archive.zip"));
        tokio::fs::create_dir_all(artifacts.parts_dir())
            .await
            .unwrap();
        let part = ManifestPart {
            id: 4,
            start: 0,
            end: 8,
        };
        let part_path = ArchiveManifest::part_path(&artifacts, part.id);
        tokio::fs::write(&part_path, []).await.unwrap();
        let (pause_tx, pause_rx) = watch::channel(false);
        let (samples_tx, mut samples_rx) = mpsc::unbounded_channel();

        let worker = tokio::spawn(run_part(
            reqwest::Client::new(),
            EhCookies::default(),
            format!("{}/archive", server.uri()),
            8,
            Validator::None,
            part,
            part_path,
            0,
            7,
            None,
            pause_rx,
            samples_tx,
        ));

        let event = tokio::time::timeout(Duration::from_millis(900), samples_rx.recv())
            .await
            .expect("worker must reconcile before retry delay")
            .expect("worker sample channel must stay open");
        assert_eq!(event.sample.part_id, 4);
        assert_eq!(event.sample.generation, 7);
        assert_eq!(event.sample.durable_len, 3);
        assert_eq!(event.sample.window_delta, 3);
        assert!(event.sample.elapsed < Duration::from_secs(1));
        assert!(!event.sample.is_rate_eligible());

        pause_tx.send(true).unwrap();
        event.applied.send(()).unwrap();
        assert!(matches!(worker.await.unwrap().1, PartExit::Paused { .. }));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn part_worker_coordinator_pause_does_not_consume_retry_budget() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("content-range", "bytes 0-7/8")
                    .set_body_bytes(b"abcdefgh")
                    .set_delay(Duration::from_millis(200)),
            )
            .mount(&server)
            .await;

        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("archive.zip"));
        tokio::fs::create_dir_all(artifacts.parts_dir())
            .await
            .unwrap();
        let part = ManifestPart {
            id: 4,
            start: 0,
            end: 8,
        };
        let part_path = ArchiveManifest::part_path(&artifacts, part.id);
        tokio::fs::write(&part_path, []).await.unwrap();

        let (pause_tx, pause_rx) = watch::channel(false);
        let (samples_tx, _samples_rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(run_part(
            reqwest::Client::new(),
            EhCookies::default(),
            format!("{}/archive", server.uri()),
            8,
            Validator::None,
            part.clone(),
            part_path.clone(),
            3,
            7,
            None,
            pause_rx,
            samples_tx,
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if server.received_requests().await.unwrap().len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("worker must send the delayed request before being paused");
        pause_tx.send(true).unwrap();
        let (_, paused) = worker.await.unwrap();
        let attempts_used = match paused {
            PartExit::Paused { attempts_used } => attempts_used,
            other => panic!("expected paused worker, got {other:?}"),
        };
        assert_eq!(attempts_used, 3);

        let (_resume_pause_tx, resume_pause_rx) = watch::channel(false);
        let (samples_tx, mut samples_rx) = mpsc::unbounded_channel();
        let resumed_worker = tokio::spawn(run_part(
            reqwest::Client::new(),
            EhCookies::default(),
            format!("{}/archive", server.uri()),
            8,
            Validator::None,
            part,
            part_path,
            attempts_used,
            8,
            None,
            resume_pause_rx,
            samples_tx,
        ));

        let event = samples_rx
            .recv()
            .await
            .expect("completed part must reconcile");
        event.applied.send(()).unwrap();
        assert!(matches!(
            resumed_worker.await.unwrap().1,
            PartExit::Complete { attempts_used: 4 }
        ));
    }
}
