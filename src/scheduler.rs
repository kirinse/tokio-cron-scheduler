use crate::context::Context;
use crate::job_data::{JobState, JobType};
use crate::store::MetaDataStorage;
use chrono::{DateTime, Utc};
use std::ops::Add;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast::Sender;
use tokio::sync::RwLock;
use uuid::Uuid;

pub struct Scheduler {
    pub job_activation_tx: Sender<Uuid>,
    pub job_delete_tx: Sender<Uuid>,
    pub notify_tx: Sender<(Uuid, JobState)>,
    pub shutdown: Arc<RwLock<bool>>,
    pub metadata_store: Arc<RwLock<Box<dyn MetaDataStorage + Send + Sync>>>,
}

impl Scheduler {
    fn new(
        context: &Context,
        metadata_store: Arc<RwLock<Box<dyn MetaDataStorage + Send + Sync>>>,
    ) -> Self {
        let job_activation_tx = context.job_activation_tx.clone();
        let notify_tx = context.notify_tx.clone();
        let job_delete_tx = context.job_delete_tx.clone();

        Self {
            job_activation_tx,
            notify_tx,
            shutdown: Arc::new(RwLock::new(false)),
            metadata_store,
            job_delete_tx,
        }
    }

    fn start(&self) {
        let job_activation_tx = self.job_activation_tx.clone();
        let job_delete_tx = self.job_delete_tx.clone();
        let notify_tx = self.notify_tx.clone();
        let shutdown = self.shutdown.clone();
        let store = self.metadata_store.clone();

        tokio::spawn(async move {
            'next_tick: loop {
                let shutdown = {
                    let r = shutdown.read().await;
                    *r
                };
                if shutdown {
                    break 'next_tick;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                let now = Utc::now();
                let next_ticks = {
                    let mut w = store.write().await;
                    w.list_next_ticks().await
                };
                if let Err(e) = next_ticks {
                    continue 'next_tick;
                }
                let mut next_ticks = next_ticks.unwrap();
                let to_be_deleted = next_ticks.iter().filter_map(|v| {
                    if v.id.is_none() {
                        return None;
                    }
                    if v.next_tick == 0 {
                        let id: Uuid = v.id.as_ref().unwrap().into();
                        Some(id)
                    } else {
                        None
                    }
                });
                for uuid in to_be_deleted {
                    let tx = job_delete_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = tx.send(uuid) {
                            eprintln!("Error sending deletion {:?}", e);
                        }
                    });
                }

                next_ticks.retain(|n| n.next_tick != 0);

                let must_runs = next_ticks.iter().filter_map(|n| {
                    let next_tick = n.next_tick_utc();
                    let last_tick = n.last_tick_utc();
                    let job_type: JobType = JobType::from_i32(n.job_type).unwrap();

                    let must_run = match (last_tick.as_ref(), next_tick.as_ref(), job_type) {
                        (None, Some(next_tick), JobType::OneShot) => {
                            let now_to_next = now.cmp(next_tick);
                            matches!(now_to_next, std::cmp::Ordering::Greater)
                                || matches!(now_to_next, std::cmp::Ordering::Equal)
                        }
                        (None, Some(next_tick), JobType::Repeated) => {
                            let now_to_next = now.cmp(next_tick);
                            matches!(now_to_next, std::cmp::Ordering::Greater)
                                || matches!(now_to_next, std::cmp::Ordering::Equal)
                        }
                        (Some(last_tick), Some(next_tick), _) => {
                            let now_to_next = now.cmp(next_tick);
                            let last_to_next = last_tick.cmp(next_tick);

                            (matches!(now_to_next, std::cmp::Ordering::Greater)
                                || matches!(now_to_next, std::cmp::Ordering::Equal))
                                && (matches!(last_to_next, std::cmp::Ordering::Less)
                                    || matches!(last_to_next, std::cmp::Ordering::Equal))
                        }
                        _ => false,
                    };
                    if must_run {
                        let id: Uuid = n.id.as_ref().map(|f| f.into()).unwrap();
                        Some(id)
                    } else {
                        None
                    }
                });

                for uuid in must_runs {
                    {
                        let tx = notify_tx.clone();
                        let uuid = uuid.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tx.send((uuid, JobState::Scheduled)) {}
                        });
                    }
                    {
                        let tx = job_activation_tx.clone();
                        let uuid = uuid.clone();
                        tokio::spawn(async move { if let Err(e) = tx.send(uuid) {} });
                    }

                    let storage = store.clone();
                    tokio::spawn(async move {
                        let mut w = storage.write().await;
                        let job = w.get(uuid.clone()).await;

                        let next_and_last_tick = match job {
                            Ok(Some(mut job)) => {
                                let job_type: JobType = JobType::from_i32(job.job_type).unwrap();
                                let schedule = job.schedule();
                                let repeated_every = job.repeated_every();
                                let next_tick = job.next_tick_utc();
                                let next_tick = match job_type {
                                    JobType::Cron => schedule.and_then(|s| s.after(&now).next()),
                                    JobType::OneShot => None,
                                    JobType::Repeated => repeated_every.and_then(|r| {
                                        next_tick.and_then(|nt| {
                                            nt.checked_add_signed(time::Duration::seconds(r as i64))
                                        })
                                    }),
                                };
                                let last_tick = Some(now);
                                Some((next_tick, last_tick))
                            }
                            _ => {
                                eprintln!("Could not get job metadata");
                                None
                            }
                        };

                        if let Some((next_tick, last_tick)) = next_and_last_tick {
                            if let Err(e) =
                                w.set_next_and_last_tick(uuid, next_tick, last_tick).await
                            {
                                eprintln!("Could not set next and last tick {:?}", e);
                            }
                        }
                    });
                }
            }
        });
    }

    async fn shutdown(&mut self) {
        let mut w = self.shutdown.write().await;
        *w = true;
    }
}