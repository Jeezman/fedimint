#![warn(clippy::pedantic)]
#![allow(clippy::ignored_unit_patterns)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

use core::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, format_err, Context as _};
use common::broken_fed_key_pair;
use db::{migrate_to_v1, DbKeyPrefix, DummyClientFundsKeyV1, DummyClientNameKey};
use fedimint_client::db::{migrate_state, ClientMigrationFn};
use fedimint_client::module::init::{ClientModuleInit, ClientModuleInitArgs};
use fedimint_client::module::recovery::NoModuleBackup;
use fedimint_client::module::{ClientContext, ClientModule, IClientModule};
use fedimint_client::sm::{Context, ModuleNotifier};
use fedimint_client::transaction::{ClientInput, ClientOutput, TransactionBuilder};
use fedimint_core::core::{Decoder, OperationId};
use fedimint_core::db::{
    Database, DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCoreTyped,
};
use fedimint_core::module::{
    ApiVersion, CommonModuleInit, ModuleCommon, ModuleInit, MultiApiVersion,
};
use fedimint_core::secp256k1::{KeyPair, PublicKey, Secp256k1};
use fedimint_core::util::{BoxStream, NextOrPending};
use fedimint_core::{apply, async_trait_maybe_send, Amount, OutPoint};
pub use fedimint_dummy_common as common;
use fedimint_dummy_common::config::DummyClientConfig;
use fedimint_dummy_common::{
    fed_key_pair, DummyCommonInit, DummyInput, DummyModuleTypes, DummyOutput, DummyOutputOutcome,
    KIND,
};
use futures::{pin_mut, FutureExt, StreamExt};
use states::DummyStateMachine;
use strum::IntoEnumIterator;

pub mod api;
pub mod db;
pub mod states;

#[derive(Debug)]
pub struct DummyClientModule {
    cfg: DummyClientConfig,
    key: KeyPair,
    notifier: ModuleNotifier<DummyStateMachine>,
    client_ctx: ClientContext<Self>,
    db: Database,
}

/// Data needed by the state machine
#[derive(Debug, Clone)]
pub struct DummyClientContext {
    pub dummy_decoder: Decoder,
}

// TODO: Boiler-plate
impl Context for DummyClientContext {}

#[apply(async_trait_maybe_send!)]
impl ClientModule for DummyClientModule {
    type Init = DummyClientInit;
    type Common = DummyModuleTypes;
    type Backup = NoModuleBackup;
    type ModuleStateMachineContext = DummyClientContext;
    type States = DummyStateMachine;

    fn context(&self) -> Self::ModuleStateMachineContext {
        DummyClientContext {
            dummy_decoder: self.decoder(),
        }
    }

    fn input_fee(&self, _input: &<Self::Common as ModuleCommon>::Input) -> Option<Amount> {
        Some(self.cfg.tx_fee)
    }

    fn output_fee(&self, _output: &<Self::Common as ModuleCommon>::Output) -> Option<Amount> {
        Some(self.cfg.tx_fee)
    }

    fn supports_being_primary(&self) -> bool {
        true
    }

    async fn create_final_inputs_and_outputs(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
        input_amount: Amount,
        output_amount: Amount,
    ) -> anyhow::Result<(
        Vec<ClientInput<DummyInput, DummyStateMachine>>,
        Vec<ClientOutput<DummyOutput, DummyStateMachine>>,
    )> {
        dbtx.ensure_isolated().expect("must be isolated");

        match input_amount.cmp(&output_amount) {
            Ordering::Less => {
                let missing_input_amount = output_amount - input_amount;

                // Check and subtract from our funds
                let our_funds = get_funds(dbtx).await;

                if our_funds < missing_input_amount {
                    return Err(format_err!("Insufficient funds"));
                }

                let updated = our_funds - missing_input_amount;

                dbtx.insert_entry(&DummyClientFundsKeyV1, &updated).await;

                let input = ClientInput {
                    input: DummyInput {
                        amount: missing_input_amount,
                        account: self.key.public_key(),
                    },
                    amount: missing_input_amount,
                    keys: vec![self.key],
                    state_machines: Arc::new(move |txid, _| {
                        vec![DummyStateMachine::Input(
                            missing_input_amount,
                            txid,
                            operation_id,
                        )]
                    }),
                };

                Ok((vec![input], Vec::new()))
            }
            Ordering::Equal => Ok((Vec::new(), Vec::new())),
            Ordering::Greater => {
                let missing_output_amount = input_amount - output_amount;
                let output = ClientOutput {
                    output: DummyOutput {
                        amount: missing_output_amount,
                        account: self.key.public_key(),
                    },
                    amount: missing_output_amount,
                    state_machines: Arc::new(move |txid, _| {
                        vec![DummyStateMachine::Output(
                            missing_output_amount,
                            txid,
                            operation_id,
                        )]
                    }),
                };

                Ok((Vec::new(), vec![output]))
            }
        }
    }

    async fn await_primary_module_output(
        &self,
        operation_id: OperationId,
        _out_point: OutPoint,
    ) -> anyhow::Result<Amount> {
        let stream = self
            .notifier
            .subscribe(operation_id)
            .await
            .filter_map(|state| async move {
                match state {
                    DummyStateMachine::OutputDone(amount, _) => Some(Ok(amount)),
                    DummyStateMachine::Refund(_) => Some(Err(anyhow::anyhow!(
                        "Error occurred processing the dummy transaction"
                    ))),
                    _ => None,
                }
            });

        pin_mut!(stream);

        stream.next_or_pending().await
    }

    async fn get_balance(&self, dbtc: &mut DatabaseTransaction<'_>) -> Amount {
        get_funds(dbtc).await
    }

    async fn subscribe_balance_changes(&self) -> BoxStream<'static, ()> {
        Box::pin(
            self.notifier
                .subscribe_all_operations()
                .filter_map(|state| async move {
                    match state {
                        DummyStateMachine::OutputDone(_, _)
                        | DummyStateMachine::Input { .. }
                        | DummyStateMachine::Refund(_) => Some(()),
                        _ => None,
                    }
                }),
        )
    }
}

impl DummyClientModule {
    pub async fn print_using_account(
        &self,
        amount: Amount,
        account_kp: KeyPair,
    ) -> anyhow::Result<(OperationId, OutPoint)> {
        let op_id = OperationId(rand::random());

        // TODO: Building a tx could be easier
        // Create input using the fed's account
        let input = ClientInput {
            input: DummyInput {
                amount,
                account: account_kp.public_key(),
            },
            amount,
            keys: vec![account_kp],
            state_machines: Arc::new(move |_, _| Vec::<DummyStateMachine>::new()),
        };

        // Build and send tx to the fed
        // Will output to our primary client module
        let tx = TransactionBuilder::new().with_input(self.client_ctx.make_client_input(input));
        let outpoint = |txid, _| OutPoint { txid, out_idx: 0 };
        let (_, change) = self
            .client_ctx
            .finalize_and_submit_transaction(op_id, KIND.as_str(), outpoint, tx)
            .await?;

        // Wait for the output of the primary module
        self.client_ctx
            .await_primary_module_outputs(op_id, change.clone())
            .await
            .context("Waiting for the output of print_using_account")?;

        Ok((op_id, change[0]))
    }

    /// Request the federation prints money for us
    pub async fn print_money(&self, amount: Amount) -> anyhow::Result<(OperationId, OutPoint)> {
        self.print_using_account(amount, fed_key_pair()).await
    }

    /// Use a broken printer to print a liability instead of money
    /// If the federation is honest, should always fail
    pub async fn print_liability(&self, amount: Amount) -> anyhow::Result<(OperationId, OutPoint)> {
        self.print_using_account(amount, broken_fed_key_pair())
            .await
    }

    /// Send money to another user
    pub async fn send_money(&self, account: PublicKey, amount: Amount) -> anyhow::Result<OutPoint> {
        self.db.ensure_isolated().expect("must be isolated");

        let op_id = OperationId(rand::random());

        // Create output using another account
        let output = ClientOutput {
            output: DummyOutput { amount, account },
            amount,
            state_machines: Arc::new(move |_, _| Vec::<DummyStateMachine>::new()),
        };

        // Build and send tx to the fed
        let tx = TransactionBuilder::new().with_output(self.client_ctx.make_client_output(output));

        let outpoint = |txid, _| OutPoint { txid, out_idx: 0 };
        let (txid, _) = self
            .client_ctx
            .finalize_and_submit_transaction(op_id, DummyCommonInit::KIND.as_str(), outpoint, tx)
            .await?;

        let tx_subscription = self.client_ctx.transaction_updates(op_id).await;

        tx_subscription
            .await_tx_accepted(txid)
            .await
            .map_err(|e| anyhow!(e))?;

        Ok(OutPoint { txid, out_idx: 0 })
    }

    /// Wait to receive money at an outpoint
    pub async fn receive_money(&self, outpoint: OutPoint) -> anyhow::Result<()> {
        let mut dbtx = self.db.begin_transaction().await;
        let DummyOutputOutcome(new_balance, account) = self
            .client_ctx
            .global_api()
            .await_output_outcome(outpoint, Duration::from_secs(10), &self.decoder())
            .await?;

        if account != self.key.public_key() {
            return Err(format_err!("Wrong account id"));
        }

        dbtx.insert_entry(&DummyClientFundsKeyV1, &new_balance)
            .await;
        dbtx.commit_tx().await;
        Ok(())
    }

    /// Return our account
    pub fn account(&self) -> PublicKey {
        self.key.public_key()
    }
}

async fn get_funds(dbtx: &mut DatabaseTransaction<'_>) -> Amount {
    let funds = dbtx.get_value(&DummyClientFundsKeyV1).await;
    funds.unwrap_or(Amount::ZERO)
}

#[derive(Debug, Clone)]
pub struct DummyClientInit;

// TODO: Boilerplate-code
impl ModuleInit for DummyClientInit {
    type Common = DummyCommonInit;
    const DATABASE_VERSION: DatabaseVersion = DatabaseVersion(2);

    async fn dump_database(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        prefix_names: Vec<String>,
    ) -> Box<dyn Iterator<Item = (String, Box<dyn erased_serde::Serialize + Send>)> + '_> {
        let mut items: BTreeMap<String, Box<dyn erased_serde::Serialize + Send>> = BTreeMap::new();
        let filtered_prefixes = DbKeyPrefix::iter().filter(|f| {
            prefix_names.is_empty() || prefix_names.contains(&f.to_string().to_lowercase())
        });

        for table in filtered_prefixes {
            match table {
                DbKeyPrefix::ClientFunds => {
                    if let Some(funds) = dbtx.get_value(&DummyClientFundsKeyV1).await {
                        items.insert("Dummy Funds".to_string(), Box::new(funds));
                    }
                }
                DbKeyPrefix::ClientName => {
                    if let Some(name) = dbtx.get_value(&DummyClientNameKey).await {
                        items.insert("Dummy Name".to_string(), Box::new(name));
                    }
                }
            }
        }

        Box::new(items.into_iter())
    }
}

/// Generates the client module
#[apply(async_trait_maybe_send!)]
impl ClientModuleInit for DummyClientInit {
    type Module = DummyClientModule;

    fn supported_api_versions(&self) -> MultiApiVersion {
        MultiApiVersion::try_from_iter([ApiVersion { major: 0, minor: 0 }])
            .expect("no version conflicts")
    }

    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(DummyClientModule {
            cfg: args.cfg().clone(),
            key: args
                .module_root_secret()
                .clone()
                .to_secp_key(&Secp256k1::new()),

            notifier: args.notifier().clone(),
            client_ctx: args.context(),
            db: args.db().clone(),
        })
    }

    fn get_database_migrations(&self) -> BTreeMap<DatabaseVersion, ClientMigrationFn> {
        let mut migrations: BTreeMap<DatabaseVersion, ClientMigrationFn> = BTreeMap::new();
        migrations.insert(DatabaseVersion(0), move |dbtx, _, _| {
            migrate_to_v1(dbtx).boxed()
        });

        migrations.insert(
            DatabaseVersion(1),
            move |_, active_states, inactive_states| {
                migrate_state(active_states, inactive_states, db::get_v1_migrated_state).boxed()
            },
        );

        migrations
    }
}
