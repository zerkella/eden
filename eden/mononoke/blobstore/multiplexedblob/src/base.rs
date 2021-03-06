/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Error;
use blobstore::{Blobstore, BlobstoreGetData};
use blobstore_stats::{record_get_stats, record_put_stats, OperationType};
use blobstore_sync_queue::OperationKey;
use cloned::cloned;
use context::{CoreContext, PerfCounterType};
use futures::{
    future::{join_all, select, BoxFuture, Either as FutureEither, FutureExt},
    stream::{FuturesUnordered, StreamExt, TryStreamExt},
};
use futures_stats::TimedFutureExt;
use itertools::{Either, Itertools};
use metaconfig_types::{BlobstoreId, MultiplexId};
use mononoke_types::BlobstoreBytes;
use scuba::ScubaSampleBuilder;
use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    iter::Iterator,
    num::NonZeroU64,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use thiserror::Error;
use time_ext::DurationExt;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

type BlobstoresWithEntry = HashSet<BlobstoreId>;
type BlobstoresReturnedNone = HashSet<BlobstoreId>;
type BlobstoresReturnedError = HashMap<BlobstoreId, Error>;

#[derive(Error, Debug, Clone)]
pub enum ErrorKind {
    #[error("Some blobstores failed, and other returned None: {0:?}")]
    SomeFailedOthersNone(Arc<BlobstoresReturnedError>),
    #[error("All blobstores failed: {0:?}")]
    AllFailed(Arc<BlobstoresReturnedError>),
    // Errors below this point are from ScrubBlobstore only. If they include an
    // Option<BlobstoreBytes>, this implies that this error is recoverable
    #[error(
        "Different blobstores have different values for this item: {0:?} differ, {1:?} do not have"
    )]
    ValueMismatch(Arc<BlobstoresWithEntry>, Arc<BlobstoresReturnedNone>),
    #[error("Some blobstores missing this item: {0:?}")]
    SomeMissingItem(Arc<BlobstoresReturnedNone>, Option<BlobstoreGetData>),
}

/// This handler is called on each successful put to underlying blobstore,
/// for put to be considered successful this handler must return success.
/// It will be used to keep self-healing table up to date.
pub trait MultiplexedBlobstorePutHandler: Send + Sync {
    fn on_put<'out>(
        &'out self,
        ctx: &'out CoreContext,
        blobstore_id: BlobstoreId,
        multiplex_id: MultiplexId,
        operation_key: &'out OperationKey,
        key: &'out str,
    ) -> BoxFuture<'out, Result<(), Error>>;
}

pub struct MultiplexedBlobstoreBase {
    multiplex_id: MultiplexId,
    blobstores: Arc<[(BlobstoreId, Arc<dyn Blobstore>)]>,
    handler: Arc<dyn MultiplexedBlobstorePutHandler>,
    scuba: ScubaSampleBuilder,
    scuba_sample_rate: NonZeroU64,
}

impl MultiplexedBlobstoreBase {
    pub fn new(
        multiplex_id: MultiplexId,
        blobstores: Vec<(BlobstoreId, Arc<dyn Blobstore>)>,
        handler: Arc<dyn MultiplexedBlobstorePutHandler>,
        mut scuba: ScubaSampleBuilder,
        scuba_sample_rate: NonZeroU64,
    ) -> Self {
        scuba.add_common_server_data();

        Self {
            multiplex_id,
            blobstores: blobstores.into(),
            handler,
            scuba,
            scuba_sample_rate,
        }
    }

    pub async fn scrub_get(
        &self,
        ctx: &CoreContext,
        key: &String,
    ) -> Result<Option<BlobstoreGetData>, ErrorKind> {
        let mut scuba = self.scuba.clone();
        scuba.sampled(self.scuba_sample_rate);

        let results = join_all(multiplexed_get(
            ctx,
            self.blobstores.as_ref(),
            key,
            OperationType::ScrubGet,
            scuba,
        ))
        .await;

        let (successes, errors): (HashMap<_, _>, HashMap<_, _>) =
            results.into_iter().partition_map(|(id, r)| match r {
                Ok(v) => Either::Left((id, v)),
                Err(v) => Either::Right((id, v)),
            });

        if successes.is_empty() {
            return Err(ErrorKind::AllFailed(errors.into()));
        }

        let mut best_value = None;
        let mut missing = HashSet::new();
        let mut answered = HashSet::new();
        let mut all_same = true;

        for (blobstore_id, value) in successes.into_iter() {
            if value.is_none() {
                missing.insert(blobstore_id);
            } else {
                answered.insert(blobstore_id);
                if best_value.is_none() {
                    best_value = value;
                } else if value.as_ref().map(BlobstoreGetData::as_bytes)
                    != best_value.as_ref().map(BlobstoreGetData::as_bytes)
                {
                    all_same = false;
                }
            }
        }

        match (all_same, best_value.is_some(), missing.is_empty()) {
            (false, _, _) => Err(ErrorKind::ValueMismatch(
                Arc::new(answered),
                Arc::new(missing),
            )),
            (true, false, _) => {
                if errors.is_empty() {
                    Ok(None)
                } else {
                    Err(ErrorKind::SomeFailedOthersNone(errors.into()))
                }
            }
            (true, true, false) => Err(ErrorKind::SomeMissingItem(Arc::new(missing), best_value)),
            (true, true, true) => Ok(best_value),
        }
    }
}

fn remap_timeout_result<O>(
    timeout_or_result: Result<Result<O, Error>, tokio::time::Elapsed>,
) -> Result<O, Error> {
    timeout_or_result.unwrap_or_else(|_| Err(Error::msg("blobstore operation timeout")))
}

pub async fn inner_put(
    ctx: &CoreContext,
    mut scuba: ScubaSampleBuilder,
    write_order: &AtomicUsize,
    blobstore_id: BlobstoreId,
    blobstore: &dyn Blobstore,
    key: String,
    value: BlobstoreBytes,
) -> Result<BlobstoreId, Error> {
    let size = value.len();
    let (stats, timeout_or_res) = timeout(
        REQUEST_TIMEOUT,
        blobstore.put(ctx.clone(), key.clone(), value),
    )
    .timed()
    .await;
    let result = remap_timeout_result(timeout_or_res);
    record_put_stats(
        &mut scuba,
        stats,
        result.as_ref(),
        key,
        ctx.session_id().to_string(),
        OperationType::Put,
        size,
        Some(blobstore_id),
        Some(write_order.fetch_add(1, Ordering::Relaxed) + 1),
    );
    result.map(|()| blobstore_id)
}

// Workaround for Blobstore returning a static lifetime future
async fn blobstore_get(
    ctx: CoreContext,
    blobstores: Arc<[(BlobstoreId, Arc<dyn Blobstore>)]>,
    key: String,
    scuba: ScubaSampleBuilder,
) -> Result<Option<BlobstoreGetData>, Error> {
    let is_logged = scuba.sampling().is_logged();
    let blobstores_count = blobstores.len();

    let (stats, result) = {
        let ctx = &ctx;
        async move {
            let mut errors = HashMap::new();
            ctx.perf_counters()
                .increment_counter(PerfCounterType::BlobGets);

            let mut requests: FuturesUnordered<_> = multiplexed_get(
                ctx.clone(),
                blobstores.as_ref(),
                &key,
                OperationType::Get,
                scuba,
            )
            .collect();
            while let Some(result) = requests.next().await {
                match result {
                    (_, Ok(Some(mut value))) => {
                        if is_logged {
                            // Allow the other requests to complete so that we can record some
                            // metrics for the blobstore.
                            tokio::spawn(requests.for_each(|_| async {}));
                        }
                        // Return the blob that won the race
                        value.remove_ctime();
                        return Ok(Some(value));
                    }
                    (blobstore_id, Err(error)) => {
                        errors.insert(blobstore_id, error);
                    }
                    (_, Ok(None)) => (),
                }
            }

            if errors.is_empty() {
                // All blobstores must have returned None, as Some would have triggered a return,
                Ok(None)
            } else {
                if errors.len() == blobstores_count {
                    Err(ErrorKind::AllFailed(Arc::new(errors)))
                } else {
                    Err(ErrorKind::SomeFailedOthersNone(Arc::new(errors)))
                }
            }
        }
        .timed()
        .await
    };

    ctx.perf_counters().set_max_counter(
        PerfCounterType::BlobGetsMaxLatency,
        stats.completion_time.as_millis_unchecked() as i64,
    );
    Ok(result?)
}

fn spawn_stream_completion(s: impl StreamExt + Send + 'static) {
    tokio::spawn(s.for_each(|_| async {}));
}

async fn select_next<F1: Future, F2: Future>(
    left: &mut FuturesUnordered<F1>,
    right: &mut FuturesUnordered<F2>,
) -> Option<Either<F1::Output, F2::Output>> {
    use Either::*;
    // Can't use a match block because that infers the wrong Send + Sync bounds for this future
    if left.is_empty() && right.is_empty() {
        None
    } else if right.is_empty() {
        left.next().await.map(Left)
    } else if left.is_empty() {
        right.next().await.map(Right)
    } else {
        use Either::*;
        // Although we drop the second element in the pair returned by select (which represents
        // the unfinished future), this does not cause data loss, because until that future is
        // awaited, it won't pull data out of the stream.
        match select(left.next(), right.next()).await {
            FutureEither::Left((None, other)) => other.await.map(Right),
            FutureEither::Right((None, other)) => other.await.map(Left),
            FutureEither::Left((Some(res), _)) => Some(Left(res)),
            FutureEither::Right((Some(res), _)) => Some(Right(res)),
        }
    }
}

impl Blobstore for MultiplexedBlobstoreBase {
    fn get(
        &self,
        ctx: CoreContext,
        key: String,
    ) -> BoxFuture<'static, Result<Option<BlobstoreGetData>, Error>> {
        let mut scuba = self.scuba.clone();
        let blobstores = self.blobstores.clone();
        scuba.sampled(self.scuba_sample_rate);

        async move { blobstore_get(ctx, blobstores, key, scuba).await }.boxed()
    }

    fn put(
        &self,
        ctx: CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> BoxFuture<'static, Result<(), Error>> {
        let write_order = Arc::new(AtomicUsize::new(0));
        let operation_key = OperationKey::gen();

        let mut puts: FuturesUnordered<_> = self
            .blobstores
            .iter()
            .cloned()
            .map({
                |(blobstore_id, blobstore)| {
                    cloned!(
                        self.handler,
                        self.multiplex_id,
                        self.scuba,
                        ctx,
                        write_order,
                        key,
                        value,
                        operation_key
                    );
                    async move {
                        inner_put(
                            &ctx,
                            scuba,
                            write_order.as_ref(),
                            blobstore_id,
                            blobstore.as_ref(),
                            key.clone(),
                            value,
                        )
                        .await?;
                        // Return the on_put handler
                        Ok(async move {
                            handler
                                .on_put(&ctx, blobstore_id, multiplex_id, &operation_key, &key)
                                .await
                        })
                    }
                }
            })
            .collect();

        async move {
            let (stats, result) = {
                let ctx = &ctx;
                async move {
                    ctx.perf_counters()
                        .increment_counter(PerfCounterType::BlobPuts);

                    // TODO: Gather all the errors for presentation to the user in a failure case
                    let mut last_err = None;
                    let mut handlers = FuturesUnordered::new();

                    while let Some(result) = select_next(&mut puts, &mut handlers).await {
                        use Either::*;
                        match result {
                            Left(Ok(handler)) => {
                                handlers.push(handler);
                                // All puts have succeeded, no errors - we're done
                                if puts.is_empty() && last_err.is_none() {
                                    // Spawn off the handlers to ensure that all writes are logged.
                                    spawn_stream_completion(handlers);
                                    return Ok(());
                                }
                            }
                            Left(Err(e)) => last_err = Some(e),
                            Right(Ok(())) => {
                                // A handler was successful. Spawn off remaining puts and handler
                                // writes, then done
                                spawn_stream_completion(puts.and_then(|handler| handler));
                                spawn_stream_completion(handlers);
                                return Ok(());
                            }
                            Right(Err(e)) => last_err = Some(e),
                        }
                    }
                    // Unwrap is safe here, because the only way to get here is if there's an Error above
                    Err(last_err.unwrap())
                }
                .timed()
                .await
            };

            ctx.perf_counters().set_max_counter(
                PerfCounterType::BlobPutsMaxLatency,
                stats.completion_time.as_millis_unchecked() as i64,
            );
            result
        }
        .boxed()
    }

    fn is_present(&self, ctx: CoreContext, key: String) -> BoxFuture<'static, Result<bool, Error>> {
        let blobstores_count = self.blobstores.len();

        let mut requests: FuturesUnordered<_> = self
            .blobstores
            .iter()
            .cloned()
            .map(|(blobstore_id, blobstore)| {
                let ctx = ctx.clone();
                let key = key.clone();
                async move { (blobstore_id, blobstore.is_present(ctx, key).await) }
            })
            .collect();

        async move {
            let (stats, result) = {
                let ctx = &ctx;
                async move {
                    let mut errors = HashMap::new();
                    ctx.perf_counters()
                        .increment_counter(PerfCounterType::BlobPresenceChecks);
                    while let Some(result) = requests.next().await {
                        match result {
                            (_, Ok(true)) => {
                                return Ok(true);
                            }
                            (blobstore_id, Err(error)) => {
                                errors.insert(blobstore_id, error);
                            }
                            (_, Ok(false)) => (),
                        }
                    }
                    if errors.is_empty() {
                        Ok(false)
                    } else {
                        if errors.len() == blobstores_count {
                            Err(ErrorKind::AllFailed(Arc::new(errors)))
                        } else {
                            Err(ErrorKind::SomeFailedOthersNone(Arc::new(errors)))
                        }
                    }
                }
                .timed()
                .await
            };
            ctx.perf_counters().set_max_counter(
                PerfCounterType::BlobPresenceChecksMaxLatency,
                stats.completion_time.as_millis_unchecked() as i64,
            );
            Ok(result?)
        }
        .boxed()
    }
}

impl fmt::Debug for MultiplexedBlobstoreBase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MultiplexedBlobstoreBase: multiplex_id: {}",
            &self.multiplex_id
        )?;
        f.debug_map()
            .entries(self.blobstores.iter().map(|(ref k, ref v)| (k, v)))
            .finish()
    }
}

async fn multiplexed_get_one(
    ctx: impl Borrow<CoreContext>,
    blobstore: Arc<dyn Blobstore>,
    blobstore_id: BlobstoreId,
    key: String,
    operation: OperationType,
    mut scuba: ScubaSampleBuilder,
) -> (BlobstoreId, Result<Option<BlobstoreGetData>, Error>) {
    let (stats, timeout_or_res) = timeout(
        REQUEST_TIMEOUT,
        blobstore.get(ctx.borrow().clone(), key.clone()),
    )
    .timed()
    .await;
    let result = remap_timeout_result(timeout_or_res);
    record_get_stats(
        &mut scuba,
        stats,
        result.as_ref(),
        key,
        ctx.borrow().session_id().to_string(),
        operation,
        Some(blobstore_id),
    );
    (blobstore_id, result)
}

fn multiplexed_get<'fut: 'iter, 'iter>(
    ctx: impl Borrow<CoreContext> + Clone + 'fut,
    blobstores: &'iter [(BlobstoreId, Arc<dyn Blobstore>)],
    key: &'iter String,
    operation: OperationType,
    scuba: ScubaSampleBuilder,
) -> impl Iterator<
    Item = impl Future<Output = (BlobstoreId, Result<Option<BlobstoreGetData>, Error>)> + 'fut,
> + 'iter {
    blobstores.iter().map(move |(blobstore_id, blobstore)| {
        multiplexed_get_one(
            ctx.clone(),
            blobstore.clone(),
            *blobstore_id,
            key.clone(),
            operation,
            scuba.clone(),
        )
    })
}
