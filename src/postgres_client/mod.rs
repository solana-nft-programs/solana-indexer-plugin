mod accounts;
mod block_handler;
mod slot_handler;
mod transaction_handler;

use crate::accounts_selector::AccountsSelectorConfig;
use crate::config::GeyserPluginPostgresConfig;
use crate::geyser_plugin_postgres::GeyserPluginPostgresError;
use crate::parallel_client::ParallelClient;
use crate::postgres_client::accounts::account_handler::all_account_handlers;
use crate::postgres_client::accounts::account_handler::select_account_handlers;
use crate::postgres_client::block_handler::BlockHandler;
use crate::postgres_client::slot_handler::SlotHandler;
use log::*;
use openssl::ssl::SslConnector;
use openssl::ssl::SslFiletype;
use openssl::ssl::SslMethod;
use postgres::Client;
use postgres::NoTls;
use postgres_openssl::MakeTlsConnector;
use solana_geyser_plugin_interface::geyser_plugin_interface::GeyserPluginError;
use solana_geyser_plugin_interface::geyser_plugin_interface::SlotStatus;
use solana_measure::measure::Measure;
use solana_metrics::*;
use std::collections::HashMap;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Mutex;
use std::thread;

use self::accounts::account_handler::AccountHandler;
pub use self::accounts::account_handler::AccountHandlerId;
pub use self::accounts::account_handler::DbAccountInfo;
pub use self::block_handler::DbBlockInfo;
pub use self::transaction_handler::build_db_transaction;
pub use self::transaction_handler::DbTransaction;
use self::transaction_handler::TransactionHandler;

pub struct SimplePostgresClient {
    batch_size: usize,
    slots_at_startup: HashSet<u64>,
    pending_account_updates: Vec<DbAccountInfo>,
    block_handler: BlockHandler,
    transaction_handler: TransactionHandler,
    account_handlers: HashMap<AccountHandlerId, Box<dyn AccountHandler>>,
    account_selector: Option<AccountsSelectorConfig>,
    client: Mutex<Client>,
}

pub trait PostgresClient {
    fn join(&mut self) -> thread::Result<()> {
        Ok(())
    }

    fn update_account(&mut self, account: DbAccountInfo, is_startup: bool) -> Result<(), GeyserPluginError>;

    fn update_slot_status(&mut self, slot: u64, parent: Option<u64>, status: SlotStatus) -> Result<(), GeyserPluginError>;

    fn notify_end_of_startup(&mut self) -> Result<(), GeyserPluginError>;

    fn log_transaction(&mut self, transaction_info: DbTransaction) -> Result<(), GeyserPluginError>;

    fn update_block_metadata(&mut self, block_info: DbBlockInfo) -> Result<(), GeyserPluginError>;
}

impl SimplePostgresClient {
    pub fn new(config: &GeyserPluginPostgresConfig) -> Result<Self, GeyserPluginError> {
        info!("[SimplePostgresClient] creating");
        let mut client = Self::connect_to_db(config)?;
        let block_handler = BlockHandler::new(&mut client, config)?;
        let transaction_handler = TransactionHandler::new(&mut client, config)?;
        let batch_size = config.batch_size;
        Ok(Self {
            batch_size,
            client: Mutex::new(client),
            block_handler,
            transaction_handler,
            pending_account_updates: Vec::with_capacity(batch_size),
            account_handlers: all_account_handlers(),
            account_selector: config.accounts_selector.clone(),
            slots_at_startup: HashSet::default(),
        })
    }

    pub fn connect_to_db(config: &GeyserPluginPostgresConfig) -> Result<Client, GeyserPluginError> {
        let result = match config.use_ssl {
            Some(true) => {
                if config.server_ca.is_none() {
                    let msg = "\"server_ca\" must be specified when \"use_ssl\" is set".to_string();
                    return Err(GeyserPluginError::ConfigFileReadError { msg });
                }
                if config.client_cert.is_none() {
                    let msg = "\"client_cert\" must be specified when \"use_ssl\" is set".to_string();
                    return Err(GeyserPluginError::ConfigFileReadError { msg });
                }
                if config.client_key.is_none() {
                    let msg = "\"client_key\" must be specified when \"use_ssl\" is set".to_string();
                    return Err(GeyserPluginError::ConfigFileReadError { msg });
                }
                let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
                if let Err(err) = builder.set_ca_file(config.server_ca.as_ref().unwrap()) {
                    let msg = format!(
                        "Failed to set the server certificate specified by \"server_ca\": {}. Error: ({})",
                        config.server_ca.as_ref().unwrap(),
                        err
                    );
                    return Err(GeyserPluginError::ConfigFileReadError { msg });
                }
                if let Err(err) = builder.set_certificate_file(config.client_cert.as_ref().unwrap(), SslFiletype::PEM) {
                    let msg = format!(
                        "Failed to set the client certificate specified by \"client_cert\": {}. Error: ({})",
                        config.client_cert.as_ref().unwrap(),
                        err
                    );
                    return Err(GeyserPluginError::ConfigFileReadError { msg });
                }
                if let Err(err) = builder.set_private_key_file(config.client_key.as_ref().unwrap(), SslFiletype::PEM) {
                    let msg = format!("Failed to set the client key specified by \"client_key\": {}. Error: ({})", config.client_key.as_ref().unwrap(), err);
                    return Err(GeyserPluginError::ConfigFileReadError { msg });
                }

                let mut connector = MakeTlsConnector::new(builder.build());
                connector.set_callback(|connect_config, _domain| {
                    connect_config.set_verify_hostname(false);
                    Ok(())
                });
                Client::connect(&config.connection_str, connector)
            }
            _ => Client::connect(&config.connection_str, NoTls),
        };
        match result {
            Err(err) => Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::ConnectionError {
                msg: format!("[connect_to_db] connection_str={} error={}", config.connection_str, err),
            }))),
            Ok(client) => Ok(client),
        }
    }
}

impl PostgresClient for SimplePostgresClient {
    fn update_account(&mut self, account: DbAccountInfo, is_startup: bool) -> Result<(), GeyserPluginError> {
        let account_key = bs58::encode(&account.pubkey).into_string();
        let owner_key = bs58::encode(&account.owner).into_string();
        debug!("[update_account] account=[{}] owner=[{}] slot=[{}]", account_key, owner_key, account.slot,);

        let client = &mut self.client.get_mut().unwrap();
        if is_startup {
            self.slots_at_startup.insert(account.slot as u64);
            self.pending_account_updates.push(account);
            // flush if batch size
            if self.pending_account_updates.len() >= self.batch_size {
                info!("[update_account_batch][flushing_accounts] length={}/{}", self.pending_account_updates.len(), self.batch_size);
                let query = self
                    .pending_account_updates
                    .drain(..)
                    .map(|a| {
                        select_account_handlers(&self.account_selector, &a, true)
                            .iter()
                            // map feed through relevant handlers
                            .map(|h| {
                                self.account_handlers
                                    .get(&AccountHandlerId::from_str(&h.handler_id).expect("Invalid account handler id"))
                                    .expect("Invalid handler id")
                                    .account_update(&a)
                            })
                            .collect::<Vec<String>>()
                            .join("")
                    })
                    .collect::<Vec<String>>()
                    .join("");

                if let Err(err) = client.batch_execute(&query) {
                    return Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                        msg: format!("[update_account_batch] error=[{}]", err),
                    })));
                };
            }
            return Ok(());
        }
        let query = select_account_handlers(&self.account_selector, &account, false)
            .iter()
            .map(|h| {
                self.account_handlers
                    .get(&AccountHandlerId::from_str(&h.handler_id).expect("Invalid account handler id"))
                    .expect("Invalid handler id")
                    .account_update(&account)
            })
            .collect::<Vec<String>>()
            .join("");
        if !query.is_empty() {
            return match client.batch_execute(&query) {
                Ok(_) => Ok(()),
                Err(err) => Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!("[update_account] error=[{}]", err),
                }))),
            };
        }
        Ok(())
    }

    fn update_slot_status(&mut self, slot: u64, parent: Option<u64>, status: SlotStatus) -> Result<(), GeyserPluginError> {
        info!("[update_slot_status] slot=[{:?}] status=[{:?}]", slot, status);
        let client = &mut self.client.get_mut().unwrap();
        let query = SlotHandler::update(slot, parent, status);
        if !query.is_empty() {
            return match client.batch_execute(&query) {
                Ok(_) => Ok(()),
                Err(err) => Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!("[update_slot_status] error=[{}]", err),
                }))),
            };
        }

        Ok(())
    }

    fn notify_end_of_startup(&mut self) -> Result<(), GeyserPluginError> {
        // flush accounts
        info!("[notify_end_of_startup][flushing_accounts] length={}/{}", self.pending_account_updates.len(), self.batch_size);
        let client = &mut self.client.get_mut().unwrap();
        let query = self
            .pending_account_updates
            .drain(..)
            .map(|a| {
                select_account_handlers(&self.account_selector, &a, true)
                    .iter()
                    // map feed through relevant handlers
                    .map(|h| {
                        self.account_handlers
                            .get(&AccountHandlerId::from_str(&h.handler_id).expect("Invalid account handler id"))
                            .expect("Invalid handler id")
                            .account_update(&a)
                    })
                    .collect::<Vec<String>>()
                    .join("")
            })
            .collect::<Vec<String>>()
            .join("");
        if let Err(err) = client.batch_execute(&query) {
            return Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                msg: format!("[notify_end_of_startup][flush_accounst_error] error=[{}]", err),
            })));
        };

        // flush slots sequentailly
        let mut measure = Measure::start("geyser-plugin-postgres-flush-slots-us");
        for s in &self.slots_at_startup {
            if let Err(err) = client.batch_execute(&SlotHandler::update(*s, None, SlotStatus::Rooted)) {
                return Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!("[notify_end_of_startup][flush_slots] error=[{}]", err),
                })));
            };
        }
        // flush slots in batch (too large)
        // let query = &self
        //     .slots_at_startup
        //     .drain()
        //     .map(|s| SlotHandler::update(s, None, SlotStatus::Rooted))
        //     .collect::<Vec<String>>()
        //     .join("");
        // if let Err(err) = client.batch_execute(&query) {
        //     return Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
        //         msg: format!("[notify_end_of_startup][flush_slots] error=[{}]", err),
        //     })));
        // };
        measure.stop();

        datapoint_info!(
            "geyser_plugin_notify_account_restore_from_snapshot_summary",
            ("flush_slots-us", measure.as_us(), i64),
            ("flush-slots-counts", self.slots_at_startup.len(), i64),
        );
        Ok(())
    }

    fn log_transaction(&mut self, transaction_info: DbTransaction) -> Result<(), GeyserPluginError> {
        self.transaction_handler.update(&mut self.client.get_mut().unwrap(), transaction_info)
    }

    fn update_block_metadata(&mut self, block_info: DbBlockInfo) -> Result<(), GeyserPluginError> {
        self.block_handler.update(&mut self.client.get_mut().unwrap(), block_info)
    }
}

pub struct PostgresClientBuilder {}

impl PostgresClientBuilder {
    pub fn build_pararallel_postgres_client(config: &GeyserPluginPostgresConfig) -> Result<(ParallelClient, Option<u64>), GeyserPluginError> {
        let mut client = SimplePostgresClient::connect_to_db(config)?;

        let account_handlers = all_account_handlers();
        let mut init_query = account_handlers.values().map(|a| a.init(config)).collect::<Vec<String>>().join("");
        init_query.push_str(&SlotHandler::init(config));
        init_query.push_str(&BlockHandler::init(config));
        init_query.push_str(&TransactionHandler::init(config));
        if let Err(err) = client.batch_execute(&init_query) {
            return Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                msg: format!("[build_pararallel_postgres_client] error=[{}]", err),
            })));
        };

        let batch_starting_slot = match config.skip_upsert_existing_accounts_at_startup {
            true => {
                let batch_slot_bound = SlotHandler::get_highest_available_slot(&mut client)?.saturating_sub(config.safe_batch_starting_slot_cushion);
                info!("[batch_starting_slot] bound={}", batch_slot_bound);
                Some(batch_slot_bound)
            }
            false => None,
        };

        ParallelClient::new(config).map(|v| (v, batch_starting_slot))
    }
}
