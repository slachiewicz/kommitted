use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Duration, Utc};
use tokio::sync::mpsc::Receiver;
use tokio::sync::RwLock;

use super::emitter::PartitionOffset;
use super::errors::{PartitionOffsetsError, PartitionOffsetsResult};
use super::lag_estimator::PartitionLagEstimator;

use crate::kafka_types::TopicPartition;
use crate::partition_offsets::known_offset::KnownOffset;

/// Holds the offset of all Topic Partitions in the Kafka Cluster, and can estimate lag of Consumers.
///
/// This is where a known Consumer Group, at a known offset in time, can get it's lag estimated.
pub struct PartitionOffsetsRegister {
    estimators: Arc<RwLock<HashMap<TopicPartition, RwLock<PartitionLagEstimator>>>>,
}

impl PartitionOffsetsRegister {
    /// Create a new [`Self`], able to hold the given offset history.
    ///
    /// # Arguments
    ///
    /// * `rx` - Channel [`Receiver`] for [`PartitionOffset`]
    /// * `offsets_history` - For each Topic Partition, how much offset history to hold.
    ///   History for each (`Topic, Partition`) pair is kept in a queue-like structure of this
    ///   size. Each entry in the structure is the pair (`Offset, UTC TS`): each pair represents
    ///   at what moment in time that particular offset was valid.
    pub fn new(mut rx: Receiver<PartitionOffset>, offsets_history: usize) -> Self {
        let por = Self {
            estimators: Arc::new(RwLock::new(HashMap::new())),
        };

        // A clone of the `por.estimator` will be moved into the async task
        // that updates the register.
        let estimators_clone = por.estimators.clone();

        // The Register is essentially "self updating" its data, by listening
        // on a channel for updates.
        //
        // The internal async task will terminate when the internal loop breaks:
        // that will happen when the `Receiver` `rx` receives `None`.
        // And, in turn, that will happen when the `Sender` part of the channel is dropped.
        tokio::spawn(async move {
            debug!("Begin receiving PartitionOffset updates");

            loop {
                tokio::select! {
                    Some(po) = rx.recv() => {
                        let k = TopicPartition{
                            topic: po.topic,
                            partition: po.partition,
                        };

                        // First, check if we need to create the estimator for this Key
                        let mut w_guard = estimators_clone.write().await;
                        if !w_guard.contains_key(&k) {
                            w_guard.insert(
                                k.clone(),
                                RwLock::new(PartitionLagEstimator::new(
                                    offsets_history,
                                )),
                            );
                        }

                        trace!("Updating Partition: {:?}", k);
                        // Second, update the PartitionLagEstimator for this Key
                        w_guard
                            .downgrade() //< Here the exclusive write lock, becomes a read lock
                            .get(&k)
                            .unwrap_or_else(|| panic!("{} for {:#?} could not be found: this should never happen!", std::any::type_name::<PartitionLagEstimator>(), k))
                            .write()
                            .await
                            .update(po.latest_offset, po.read_datetime);
                    },
                    else => {
                        info!("Emitters stopping: breaking (internal) loop");
                        break;
                    }
                }
            }
        });

        por
    }
}

impl PartitionOffsetsRegister {
    /// Estimate offset lag for consumer of specific [`TopicPartition`], given it's current `consumed_offset`.
    ///
    /// # Arguments
    ///
    /// * `topic_partition` - Topic Partition consumed by the Consumer
    /// * `consumed_offset` - Offset up to which the Consumer has consumed
    pub async fn estimate_offset_lag(
        &self,
        topic_partition: &TopicPartition,
        consumed_offset: u64,
    ) -> PartitionOffsetsResult<u64> {
        self.estimators
            .read()
            .await
            .get(topic_partition)
            .ok_or(PartitionOffsetsError::LagEstimatorNotFound(
                topic_partition.topic.to_string(),
                topic_partition.partition,
            ))?
            .read()
            .await
            .estimate_offset_lag(consumed_offset)
    }

    /// Estimate time lag for consumer of specific [`TopicPartition`], given it's current `consumed_offset` and `consumed_offset_datetime`.
    ///
    /// # Arguments
    ///
    /// * `topic_partition` - Topic Partition consumed by the Consumer
    /// * `consumed_offset` - Offset up to which the Consumer has consumed
    /// * `consumed_offset_datetime` - [`Datetime<Utc>`] when the `consumed_offset` was committed
    pub async fn estimate_time_lag(
        &self,
        topic_partition: &TopicPartition,
        consumed_offset: u64,
        consumed_offset_datetime: DateTime<Utc>,
    ) -> PartitionOffsetsResult<Duration> {
        self.estimators
            .read()
            .await
            .get(topic_partition)
            .ok_or(PartitionOffsetsError::LagEstimatorNotFound(
                topic_partition.topic.to_string(),
                topic_partition.partition,
            ))?
            .read()
            .await
            .estimate_time_lag(consumed_offset, consumed_offset_datetime)
    }

    /// Get the earliest known offset of specific [`TopicPartition`].
    ///
    /// # Arguments
    ///
    /// * `topic_partition` - Topic Partition we want to know the earliest offset of
    pub async fn get_earliest_offset(&self, topic_partition: &TopicPartition) -> PartitionOffsetsResult<KnownOffset> {
        self.estimators
            .read()
            .await
            .get(topic_partition)
            .ok_or(PartitionOffsetsError::LagEstimatorNotFound(
                topic_partition.topic.to_string(),
                topic_partition.partition,
            ))?
            .read()
            .await
            .earliest_offset()
            .cloned()
    }

    /// Get the latest known offset of specific [`TopicPartition`].
    ///
    /// # Arguments
    ///
    /// * `topic_partition` - Topic Partition we want to know the latest offset of
    pub async fn get_latest_offset(&self, topic_partition: &TopicPartition) -> PartitionOffsetsResult<KnownOffset> {
        self.estimators
            .read()
            .await
            .get(topic_partition)
            .ok_or(PartitionOffsetsError::LagEstimatorNotFound(
                topic_partition.topic.to_string(),
                topic_partition.partition,
            ))?
            .read()
            .await
            .latest_offset()
            .cloned()
    }

    /// Get some basic registry usage stats.
    ///
    /// Returns the usage of the internal [`PartitionLagEstimator`]s, as `(min, max, avg, count)` tuple.
    /// `count` is the number [`TopicPartition`] this registry has a [`PartitionLagEstimator`] of.
    pub async fn get_usage(&self) -> (f64, f64, f64, usize) {
        let count = self.estimators.read().await.len();

        // We have no estimators usually at launch: don't bother and return zeros
        if count == 0 {
            return (0_f64, 0_f64, 0_f64, 0);
        }

        let mut sum = 0_f64;
        let mut min = f64::MAX;
        let mut max = f64::MIN;

        for (_, est_rwlock) in self.estimators.read().await.iter() {
            let curr = est_rwlock.read().await.usage_percent();

            sum += curr;
            if curr > max {
                max = curr;
            }
            if curr < min {
                min = curr;
            }
        }

        (min, max, sum / count as f64, count)
    }
}
