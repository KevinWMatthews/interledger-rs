use bytes::Bytes;
use interledger_service::{Account, AccountStore};
use interledger_settlement::{IdempotentData, IdempotentStore};
use tokio::runtime::Runtime;

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

use hyper::StatusCode;
use std::process::Command;
use std::str::FromStr;
use std::thread::sleep;
use std::time::Duration;

use ethereum_tx_sign::web3::{
    futures::future::{err, ok, Future},
    types::{Address, U256},
};

use super::eth_engine::EthereumLedgerSettlementEngine;
use super::types::{Addresses, EthereumAccount, EthereumLedgerTxSigner, EthereumStore};

#[derive(Debug, Clone)]
pub struct TestAccount {
    pub id: u64,
    pub address: Address,
    pub token_address: Address,
    pub no_details: bool,
}

impl Account for TestAccount {
    type AccountId = u64;

    fn id(&self) -> u64 {
        self.id
    }
}

impl EthereumAccount for TestAccount {
    fn token_address(&self) -> Option<Address> {
        if self.no_details {
            return None;
        }
        Some(self.token_address)
    }
    fn own_address(&self) -> Address {
        self.address
    }
}

// Test Store
#[derive(Clone)]
pub struct TestStore {
    pub accounts: Arc<Vec<TestAccount>>,
    pub should_fail: bool,
    pub addresses: Arc<RwLock<HashMap<u64, Addresses>>>,
    pub address_to_id: Arc<RwLock<HashMap<Addresses, u64>>>,
    #[allow(clippy::all)]
    pub cache: Arc<RwLock<HashMap<String, (StatusCode, String, [u8; 32])>>>,
    pub last_observed_block: Arc<RwLock<U256>>,
    pub last_observed_balance: Arc<RwLock<U256>>,
    pub cache_hits: Arc<RwLock<u64>>,
}

impl EthereumStore for TestStore {
    type Account = TestAccount;

    fn save_account_addresses(
        &self,
        account_ids: Vec<u64>,
        data: Vec<Addresses>,
    ) -> Box<Future<Item = (), Error = ()> + Send> {
        let mut guard = self.addresses.write();
        let mut guard2 = self.address_to_id.write();
        for (acc, d) in account_ids.into_iter().zip(data.into_iter()) {
            (*guard).insert(acc, d);
            (*guard2).insert(d, acc);
        }
        Box::new(ok(()))
    }

    fn load_account_addresses(
        &self,
        account_ids: Vec<u64>,
    ) -> Box<dyn Future<Item = Vec<Addresses>, Error = ()> + Send> {
        let mut v = Vec::with_capacity(account_ids.len());
        let addresses = self.addresses.read();
        for acc in &account_ids {
            if let Some(d) = addresses.get(&acc) {
                v.push(Addresses {
                    own_address: d.own_address,
                    token_address: d.token_address,
                });
            } else {
                // if the account is not found, error out
                return Box::new(err(()));
            }
        }
        Box::new(ok(v))
    }

    fn save_recently_observed_data(
        &self,
        block: U256,
        balance: U256,
    ) -> Box<dyn Future<Item = (), Error = ()> + Send> {
        let mut guard = self.last_observed_block.write();
        *guard = block;
        let mut guard = self.last_observed_balance.write();
        *guard = balance;
        Box::new(ok(()))
    }

    fn load_recently_observed_data(
        &self,
    ) -> Box<dyn Future<Item = (U256, U256), Error = ()> + Send> {
        Box::new(ok((
            *self.last_observed_block.read(),
            *self.last_observed_balance.read(),
        )))
    }

    fn load_account_id_from_address(
        &self,
        eth_address: Addresses,
    ) -> Box<dyn Future<Item = u64, Error = ()> + Send> {
        let addresses = self.address_to_id.read();
        let d = if let Some(d) = addresses.get(&eth_address) {
            *d
        } else {
            return Box::new(err(()));
        };

        Box::new(ok(d))
    }
}

impl AccountStore for TestStore {
    type Account = TestAccount;

    fn get_accounts(
        &self,
        account_ids: Vec<<<Self as AccountStore>::Account as Account>::AccountId>,
    ) -> Box<Future<Item = Vec<Self::Account>, Error = ()> + Send> {
        let accounts: Vec<TestAccount> = self
            .accounts
            .iter()
            .filter_map(|account| {
                if account_ids.contains(&account.id) {
                    Some(account.clone())
                } else {
                    None
                }
            })
            .collect();
        if accounts.len() == account_ids.len() {
            Box::new(ok(accounts))
        } else {
            Box::new(err(()))
        }
    }
}

impl IdempotentStore for TestStore {
    fn load_idempotent_data(
        &self,
        idempotency_key: String,
    ) -> Box<dyn Future<Item = Option<IdempotentData>, Error = ()> + Send> {
        let cache = self.cache.read();
        let d = if let Some(data) = cache.get(&idempotency_key) {
            let mut guard = self.cache_hits.write();
            *guard += 1; // used to test how many times this branch gets executed
            Some((data.0, Bytes::from(data.1.clone()), data.2))
        } else {
            None
        };

        Box::new(ok(d))
    }

    fn save_idempotent_data(
        &self,
        idempotency_key: String,
        input_hash: [u8; 32],
        status_code: StatusCode,
        data: Bytes,
    ) -> Box<dyn Future<Item = (), Error = ()> + Send> {
        let mut cache = self.cache.write();
        cache.insert(
            idempotency_key,
            (
                status_code,
                String::from_utf8_lossy(&data).to_string(),
                input_hash,
            ),
        );
        Box::new(ok(()))
    }
}

impl TestStore {
    pub fn new(accs: Vec<TestAccount>, should_fail: bool, initialize: bool) -> Self {
        let mut addresses = HashMap::new();
        let mut address_to_id = HashMap::new();
        if initialize {
            for account in &accs {
                let token_address = if !account.no_details {
                    Some(account.token_address)
                } else {
                    None
                };
                let addrs = Addresses {
                    own_address: account.address,
                    token_address,
                };
                addresses.insert(account.id, addrs);
                address_to_id.insert(addrs, account.id);
            }
        }

        TestStore {
            accounts: Arc::new(accs),
            should_fail,
            addresses: Arc::new(RwLock::new(addresses)),
            address_to_id: Arc::new(RwLock::new(address_to_id)),
            cache: Arc::new(RwLock::new(HashMap::new())),
            cache_hits: Arc::new(RwLock::new(0)),
            last_observed_balance: Arc::new(RwLock::new(U256::from(0))),
            last_observed_block: Arc::new(RwLock::new(U256::from(0))),
        }
    }
}

// Test Service

impl TestAccount {
    pub fn new(id: u64, address: &str, token_address: &str) -> Self {
        Self {
            id,
            address: Address::from_str(address).unwrap(),
            token_address: Address::from_str(token_address).unwrap(),
            no_details: false,
        }
    }
}

// Helper to create a new engine and spin a new ganache instance.
pub fn test_engine<Si, S, A>(
    store: S,
    key: Si,
    confs: usize,
    connector_url: String,
    watch_incoming: bool,
) -> EthereumLedgerSettlementEngine<S, Si, A>
where
    Si: EthereumLedgerTxSigner + Clone + Send + Sync + 'static,
    S: EthereumStore<Account = A> + IdempotentStore + Clone + Send + Sync + 'static,
    A: EthereumAccount + Send + Sync + 'static,
{
    let chain_id = 1;
    let poll_frequency = Duration::from_secs(1);
    EthereumLedgerSettlementEngine::new(
        "http://localhost:8545".to_string(),
        store,
        key,
        chain_id,
        confs,
        poll_frequency,
        connector_url.parse().unwrap(),
        None,
        watch_incoming,
    )
}

pub fn start_ganache() -> std::process::Child {
    let mut ganache = Command::new("ganache-cli");
    let ganache = ganache.stdout(std::process::Stdio::null()).arg("-m").arg(
        "abstract vacuum mammal awkward pudding scene penalty purchase dinner depart evoke puzzle",
    );
    let ganache_pid = ganache.spawn().expect("couldnt start ganache-cli");
    // wait a couple of seconds for ganache to boot up
    sleep(Duration::from_secs(5));
    ganache_pid
}

pub fn test_store(
    account: TestAccount,
    store_fails: bool,
    account_has_engine: bool,
    initialize: bool,
) -> TestStore {
    let mut acc = account.clone();
    acc.no_details = !account_has_engine;
    TestStore::new(vec![acc], store_fails, initialize)
}

// Futures helper taken from the store_helpers in interledger-store-redis.
pub fn block_on<F>(f: F) -> Result<F::Item, F::Error>
where
    F: Future + Send + 'static,
    F::Item: Send,
    F::Error: Send,
{
    // Only run one test at a time
    let _ = env_logger::try_init();
    let mut runtime = Runtime::new().unwrap();
    runtime.block_on(f)
}