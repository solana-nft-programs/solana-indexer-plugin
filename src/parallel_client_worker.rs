use crate::abort;
use crate::config::GeyserPluginPostgresConfig;
use crate::postgres_client::DbAccountInfo;
use crate::postgres_client::DbBlockInfo;
use crate::postgres_client::DbTransaction;
use crate::postgres_client::PostgresClient;
use crate::postgres_client::SimplePostgresClient;
use crossbeam_channel::Receiver;
use crossbeam_channel::RecvTimeoutError;
use log::*;
use solana_geyser_plugin_interface::geyser_plugin_interface::GeyserPluginError;
use solana_geyser_plugin_interface::geyser_plugin_interface::SlotStatus;
use solana_measure::measure::Measure;
use solana_metrics::*;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub struct UpdateAccountRequest {
    pub account: DbAccountInfo,
    pub is_startup: bool,
}

pub struct UpdateSlotRequest {
    pub slot: u64,
    pub parent: Option<u64>,
    pub slot_status: SlotStatus,
}

pub struct LogTransactionRequest {
    pub transaction_info: DbTransaction,
}

pub struct UpdateBlockMetadataRequest {
    pub block_info: DbBlockInfo,
}

#[warn(clippy::large_enum_variant)]
pub enum WorkRequest {
    UpdateAccount(Box<UpdateAccountRequest>),
    UpdateSlot(Box<UpdateSlotRequest>),
    LogTransaction(Box<LogTransactionRequest>),
    UpdateBlockMetadata(Box<UpdateBlockMetadataRequest>),
}

pub struct ParallelClientWorker {
    client: SimplePostgresClient,
    /// Indicating if accounts notification during startup is done.
    is_startup_done: bool,
}

impl ParallelClientWorker {
    pub fn new(config: GeyserPluginPostgresConfig) -> Result<Self, GeyserPluginError> {
        let result = SimplePostgresClient::new(&config);
        match result {
            Ok(client) => Ok(ParallelClientWorker { client, is_startup_done: false }),
            Err(err) => {
                error!("[ParallelClientWorker] error=[{}]", err);
                Err(err)
            }
        }
    }

    pub fn do_work(
        &mut self,
        receiver: Receiver<WorkRequest>,
        exit_worker: Arc<AtomicBool>,
        is_startup_done: Arc<AtomicBool>,
        startup_done_count: Arc<AtomicUsize>,
        panic_on_db_errors: bool,
    ) -> Result<(), GeyserPluginError> {
        while !exit_worker.load(Ordering::Relaxed) {
            let mut measure = Measure::start("geyser-plugin-postgres-worker-recv");
            let work = receiver.recv_timeout(Duration::from_millis(500));
            measure.stop();
            inc_new_counter_debug!("geyser-plugin-postgres-worker-recv-us", measure.as_us() as usize, 100000, 100000);
            match work {
                Ok(work) => match work {
                    WorkRequest::UpdateAccount(request) => {
                        if let Err(err) = self.client.update_account(request.account, request.is_startup) {
                            error!("Failed to update account: ({})", err);
                            if panic_on_db_errors {
                                abort();
                            }
                        }
                    }
                    WorkRequest::UpdateSlot(request) => {
                        if let Err(err) = self.client.update_slot_status(request.slot, request.parent, request.slot_status) {
                            error!("Failed to update slot: ({})", err);
                            if panic_on_db_errors {
                                abort();
                            }
                        }
                    }
                    WorkRequest::LogTransaction(transaction_log_info) => {
                        if let Err(err) = self.client.log_transaction(transaction_log_info.transaction_info) {
                            error!("Failed to update transaction: ({})", err);
                            if panic_on_db_errors {
                                abort();
                            }
                        }
                    }
                    WorkRequest::UpdateBlockMetadata(block_info) => {
                        if let Err(err) = self.client.update_block_metadata(block_info.block_info) {
                            error!("Failed to update block metadata: ({})", err);
                            if panic_on_db_errors {
                                abort();
                            }
                        }
                    }
                },
                Err(err) => match err {
                    RecvTimeoutError::Timeout => {
                        if !self.is_startup_done && is_startup_done.load(Ordering::Relaxed) {
                            if let Err(err) = self.client.notify_end_of_startup() {
                                error!("Error in notifying end of startup: ({})", err);
                                if panic_on_db_errors {
                                    abort();
                                }
                            }
                            self.is_startup_done = true;
                            startup_done_count.fetch_add(1, Ordering::Relaxed);
                        }

                        continue;
                    }
                    _ => {
                        error!("[error] {:?} {:?}", err, panic_on_db_errors);
                        if panic_on_db_errors {
                            abort();
                        }
                        break;
                    }
                },
            }
        }
        Ok(())
    }
}
