use core::fmt;
use std::any::Any;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::Arc;
use std::{ffi, marker, ops};

use anyhow::{anyhow, bail};
use fedimint_api_client::api::DynGlobalApi;
use fedimint_core::config::ClientConfig;
use fedimint_core::core::{
    Decoder, DynInput, DynOutput, IntoDynInstance, ModuleInstanceId, ModuleKind, OperationId,
};
use fedimint_core::db::{AutocommitError, Database, DatabaseTransaction, PhantomBound};
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::registry::{ModuleDecoderRegistry, ModuleRegistry};
use fedimint_core::module::{CommonModuleInit, ModuleCommon, ModuleInit};
use fedimint_core::task::{MaybeSend, MaybeSync};
use fedimint_core::util::{BoxFuture, BoxStream};
use fedimint_core::{
    apply, async_trait_maybe_send, dyn_newtype_define, maybe_add_send_sync, Amount, OutPoint,
    TransactionId,
};
use secp256k1_zkp::PublicKey;

use self::init::ClientModuleInit;
use crate::module::recovery::{DynModuleBackup, ModuleBackup};
use crate::sm::{self, ActiveStateMeta, Context, DynContext, DynState, State};
use crate::transaction::{ClientInput, ClientOutput, TransactionBuilder};
use crate::{oplog, AddStateMachinesResult, Client, ClientStrong, ClientWeak, TransactionUpdates};

pub mod init;
pub mod recovery;

pub type ClientModuleRegistry = ModuleRegistry<DynClientModule>;

/// A final, fully initialized [`crate::Client`]
///
/// Client modules need to be able to access a `Client` they are a part
/// of. To break the circular dependency, the final `Client` is passed
/// after `Client` was built via a shared state.
#[derive(Clone, Default)]
pub struct FinalClient(Arc<std::sync::OnceLock<ClientWeak>>);

impl FinalClient {
    /// Get a temporary [`ClientStrong`]
    ///
    /// Care must be taken to not let the user take ownership of this value,
    /// and not store it elsewhere permanently either, as it could prevent
    /// the cleanup of the Client.
    pub(crate) fn get(&self) -> ClientStrong {
        self.0
            .get()
            .expect("client must be already set")
            .upgrade()
            .expect("client module context must not be use past client shutdown")
    }

    pub(crate) fn set(&self, client: ClientWeak) {
        self.0.set(client).expect("FinalLazyClient already set");
    }
}

/// A Client context for a [`ClientModule`] `M`
///
/// Client modules can interact with the whole
/// client through this struct.
pub struct ClientContext<M> {
    client: FinalClient,
    module_instance_id: ModuleInstanceId,
    module_db: Database,
    _marker: marker::PhantomData<M>,
}

impl<M> Clone for ClientContext<M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            module_db: self.module_db.clone(),
            module_instance_id: self.module_instance_id,
            _marker: marker::PhantomData,
        }
    }
}

/// A reference back to itself that the module cacn get from the
/// [`ClientContext`]
pub struct ClientContextSelfRef<'s, M> {
    // we are OK storing `ClientStrong` here, because of the `'s` preventing `Self` from being
    // stored permanently somewhere
    client: ClientStrong,
    module_instance_id: ModuleInstanceId,
    _marker: marker::PhantomData<&'s M>,
}

impl<M> ops::Deref for ClientContextSelfRef<'_, M>
where
    M: ClientModule,
{
    type Target = M;

    fn deref(&self) -> &Self::Target {
        self.client
            .get_module(self.module_instance_id)
            .as_any()
            .downcast_ref::<M>()
            .unwrap_or_else(|| panic!("Module is not of type {}", std::any::type_name::<M>()))
    }
}

impl<M> fmt::Debug for ClientContext<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientContext")
    }
}

/// A context of a database transaction started with
/// `ClientContext::module_autocommit`
pub struct ClientDbTxContext<'r, 'o, M> {
    dbtx: &'r mut DatabaseTransaction<'o>,
    client: &'r ClientContext<M>,
}

impl<'r, 'o, M> ClientDbTxContext<'r, 'o, M>
where
    M: ClientModule,
{
    /// Get a reference to [`DatabaseTransaction`] isolated database transaction
    pub fn module_dbtx(&mut self) -> DatabaseTransaction<'_> {
        self.dbtx
            .to_ref_with_prefix_module_id(self.client.module_instance_id)
    }

    pub fn client_ctx(&self) -> &ClientContext<M> {
        self.client
    }

    pub async fn add_state_machines(
        &mut self,
        dyn_states: Vec<DynState>,
    ) -> AddStateMachinesResult {
        self.client
            .client
            .get()
            .add_state_machines(self.dbtx, dyn_states)
            .await
    }

    pub fn decoders(&self) -> ModuleDecoderRegistry {
        self.client.client.get().decoders().clone()
    }

    pub async fn add_operation_log_entry(
        &mut self,
        operation_id: OperationId,
        operation_type: &str,
        operation_meta: impl serde::Serialize,
    ) {
        self.client
            .client
            .get()
            .operation_log()
            .add_operation_log_entry(self.dbtx, operation_id, operation_type, operation_meta)
            .await;
    }

    pub async fn add_state_machines_dbtx(
        &mut self,
        states: Vec<DynState>,
    ) -> AddStateMachinesResult {
        self.client
            .client
            .get()
            .executor
            .add_state_machines_dbtx(self.dbtx, states)
            .await
    }
}

impl<M> ClientContext<M>
where
    M: ClientModule,
{
    /// Get a reference back to client module from the [`Self`]
    ///
    /// It's often necessary for a client module to "move self"
    /// by-value, especially due to async lifetimes issues.
    /// Clients usually work with `&mut self`, which can't really
    /// work in such context.
    ///
    /// Fortunately [`ClientContext`] is `Clone` and `Send, and
    /// can be used to recover the reference to the module at later
    /// time.
    #[allow(clippy::needless_lifetimes)] // just for explicitiness
    pub fn self_ref<'s>(&'s self) -> ClientContextSelfRef<'s, M> {
        ClientContextSelfRef {
            client: self.client.get(),
            module_instance_id: self.module_instance_id,
            _marker: marker::PhantomData,
        }
    }

    /// Get a reference to a global Api handle
    pub fn global_api(&self) -> DynGlobalApi {
        self.client.get().api_clone()
    }
    pub fn decoders(&self) -> ModuleDecoderRegistry {
        self.client.get().decoders().clone()
    }

    pub fn input_from_dyn<'i>(
        &self,
        input: &'i DynInput,
    ) -> Option<&'i <M::Common as ModuleCommon>::Input> {
        (input.module_instance_id() == self.module_instance_id).then(|| {
            input
                .as_any()
                .downcast_ref::<<M::Common as ModuleCommon>::Input>()
                .expect("instance_id just checked")
        })
    }

    pub fn output_from_dyn<'o>(
        &self,
        output: &'o DynOutput,
    ) -> Option<&'o <M::Common as ModuleCommon>::Output> {
        (output.module_instance_id() == self.module_instance_id).then(|| {
            output
                .as_any()
                .downcast_ref::<<M::Common as ModuleCommon>::Output>()
                .expect("instance_id just checked")
        })
    }

    pub fn map_dyn<'s, 'i, 'o, I>(
        &'s self,
        typed: impl IntoIterator<Item = I> + 'i,
    ) -> impl Iterator<Item = <I as IntoDynInstance>::DynType> + 'o
    where
        I: IntoDynInstance,
        'i: 'o,
        's: 'o,
    {
        typed.into_iter().map(move |i| self.make_dyn(i))
    }

    /// Turn a typed output into a dyn version
    pub fn make_dyn_output(&self, output: <M::Common as ModuleCommon>::Output) -> DynOutput {
        self.make_dyn(output)
    }

    /// Turn a typed input into a dyn version
    pub fn make_dyn_input(&self, input: <M::Common as ModuleCommon>::Input) -> DynInput {
        self.make_dyn(input)
    }

    /// Turn a `typed` into a dyn version
    pub fn make_dyn<I>(&self, typed: I) -> <I as IntoDynInstance>::DynType
    where
        I: IntoDynInstance,
    {
        typed.into_dyn(self.module_instance_id)
    }

    /// Turn a typed [`ClientOutput`] into a dyn version
    pub fn make_client_output<O, S>(&self, output: ClientOutput<O, S>) -> ClientOutput
    where
        O: IntoDynInstance<DynType = DynOutput> + 'static,
        S: IntoDynInstance<DynType = DynState> + 'static,
    {
        self.make_dyn(output)
    }

    /// Turn a typed [`ClientInput`] into a dyn version
    pub fn make_client_input<O, S>(&self, input: ClientInput<O, S>) -> ClientInput
    where
        O: IntoDynInstance<DynType = DynInput> + 'static,
        S: IntoDynInstance<DynType = DynState> + 'static,
    {
        self.make_dyn(input)
    }

    pub fn make_dyn_state<S>(&self, sm: S) -> DynState
    where
        S: sm::IState + 'static,
    {
        DynState::from_typed(self.module_instance_id, sm)
    }

    /// An [`Database::autocommit`] on module's own database partition
    pub async fn module_autocommit<'s, 'dbtx, F, T, E>(
        &'s self,
        tx_fn: F,
        max_attempts: Option<usize>,
    ) -> Result<T, AutocommitError<E>>
    where
        's: 'dbtx,
        for<'r, 'o> F: Fn(
                &'r mut ClientDbTxContext<'r, 'o, M>,
                PhantomBound<'dbtx, 'o>,
            ) -> BoxFuture<'r, Result<T, E>>
            + MaybeSync,
    {
        let tx_fn = &tx_fn;
        self.global_db()
            .autocommit(
                move |dbtx, _| {
                    Box::pin(async move {
                        tx_fn(&mut ClientDbTxContext { dbtx, client: self }, PhantomData).await
                    })
                },
                max_attempts,
            )
            .await
    }

    // TODO: rename `module_autocommit` to something else and make this a default
    pub async fn module_autocommit_2<'s, 'dbtx, F, T>(
        &'s self,
        tx_fn: F,
        max_attempts: Option<usize>,
    ) -> anyhow::Result<T>
    where
        's: 'dbtx,
        for<'r, 'o> F: Fn(
                &'r mut ClientDbTxContext<'r, 'o, M>,
                PhantomBound<'dbtx, 'o>,
            ) -> BoxFuture<'r, anyhow::Result<T>>
            + MaybeSync,
    {
        self.module_autocommit(tx_fn, max_attempts)
            .await
            .map_err(|e| match e {
                AutocommitError::ClosureError { error, .. } => error,
                AutocommitError::CommitFailed { last_error, .. } => {
                    panic!("Commit to DB failed: {last_error}")
                }
            })
    }

    /// See [`crate::Client::finalize_and_submit_transaction`]
    pub async fn finalize_and_submit_transaction<F, Meta>(
        &self,
        operation_id: OperationId,
        operation_type: &str,
        operation_meta: F,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<(TransactionId, Vec<OutPoint>)>
    where
        F: Fn(TransactionId, Vec<OutPoint>) -> Meta + Clone + MaybeSend + MaybeSync,
        Meta: serde::Serialize + MaybeSend,
    {
        self.client
            .get()
            .finalize_and_submit_transaction(
                operation_id,
                operation_type,
                operation_meta,
                tx_builder,
            )
            .await
    }

    /// See [`crate::Client::transaction_updates`]
    pub async fn transaction_updates(&self, operation_id: OperationId) -> TransactionUpdates {
        self.client.get().transaction_updates(operation_id).await
    }

    /// See [`crate::Client::await_primary_module_outputs`]
    pub async fn await_primary_module_outputs(
        &self,
        operation_id: OperationId,
        outputs: Vec<OutPoint>,
    ) -> anyhow::Result<Amount> {
        self.client
            .get()
            .await_primary_module_outputs(operation_id, outputs)
            .await
    }

    // TODO: unify with `Self::get_operation`
    pub async fn get_operation(
        &self,
        operation_id: OperationId,
    ) -> anyhow::Result<oplog::OperationLogEntry> {
        let operation = self
            .client
            .get()
            .operation_log()
            .get_operation(operation_id)
            .await
            .ok_or(anyhow::anyhow!("Operation not found"))?;

        if operation.operation_module_kind() != M::kind().as_str() {
            bail!("Operation is not a lightning operation");
        }

        Ok(operation)
    }

    pub fn global_db(&self) -> fedimint_core::db::Database {
        self.client.get().db().clone()
    }

    pub fn module_db(&self) -> &Database {
        &self.module_db
    }

    pub async fn has_active_states(&self, op_id: OperationId) -> bool {
        self.client.get().has_active_states(op_id).await
    }

    pub async fn operation_exists(&self, op_id: OperationId) -> bool {
        self.client.get().operation_exists(op_id).await
    }

    pub async fn get_own_active_states(&self) -> Vec<(M::States, ActiveStateMeta)> {
        self.client
            .get()
            .executor
            .get_active_states()
            .await
            .into_iter()
            .filter(|s| s.0.module_instance_id() == self.module_instance_id)
            .map(|s| {
                (
                    s.0.as_any()
                        .downcast_ref::<M::States>()
                        .expect("incorrect output type passed to module plugin")
                        .clone(),
                    s.1,
                )
            })
            .collect()
    }

    pub fn get_config(&self) -> ClientConfig {
        self.client.get().get_config().clone()
    }

    /// Returns an invite code for the federation that points to an arbitrary
    /// guardian server for fetching the config
    pub fn get_invite_code(&self) -> InviteCode {
        let cfg = self.get_config().global;
        let (any_guardian_id, any_guardian_url) = cfg
            .api_endpoints
            .iter()
            .next()
            .expect("A federation always has at least one guardian");
        InviteCode::new(
            any_guardian_url.url.clone(),
            *any_guardian_id,
            cfg.calculate_federation_id(),
            self.client.get().api_secret().clone(),
        )
    }

    pub fn get_internal_payment_markers(&self) -> anyhow::Result<(PublicKey, u64)> {
        self.client.get().get_internal_payment_markers()
    }

    /// This method starts n state machines with given operation id without a
    /// corresponding transaction
    pub async fn manual_operation_start(
        &self,
        operation_id: OperationId,
        op_type: &str,
        operation_meta: impl serde::Serialize + Debug,
        sms: Vec<DynState>,
    ) -> anyhow::Result<()> {
        let db = self.client.get().db().clone();
        let mut dbtx = db.begin_transaction().await;

        if Client::operation_exists_dbtx(&mut dbtx.to_ref_nc(), operation_id).await {
            bail!(
                "Operation with id {} already exists",
                operation_id.fmt_short()
            );
        }

        self.client
            .get()
            .operation_log
            .add_operation_log_entry(&mut dbtx.to_ref_nc(), operation_id, op_type, operation_meta)
            .await;

        self.client
            .get()
            .executor
            .add_state_machines_dbtx(&mut dbtx.to_ref_nc(), sms)
            .await
            .expect("State machine is valid");

        dbtx.commit_tx_result().await.map_err(|_| {
            anyhow!(
                "Operation with id {} already exists",
                operation_id.fmt_short()
            )
        })?;

        Ok(())
    }
}

/// Fedimint module client
#[apply(async_trait_maybe_send!)]
pub trait ClientModule: Debug + MaybeSend + MaybeSync + 'static {
    type Init: ClientModuleInit;

    /// Common module types shared between client and server
    type Common: ModuleCommon;

    /// Data stored in regular backups so that restoring doesn't have to start
    /// from epoch 0
    type Backup: ModuleBackup;

    /// Data and API clients available to state machine transitions of this
    /// module
    type ModuleStateMachineContext: Context;

    /// All possible states this client can submit to the executor
    type States: State<ModuleContext = Self::ModuleStateMachineContext>
        + IntoDynInstance<DynType = DynState>;

    fn decoder() -> Decoder {
        let mut decoder_builder = Self::Common::decoder_builder();
        decoder_builder.with_decodable_type::<Self::States>();
        decoder_builder.with_decodable_type::<Self::Backup>();
        decoder_builder.build()
    }

    fn kind() -> ModuleKind {
        <<<Self as ClientModule>::Init as ModuleInit>::Common as CommonModuleInit>::KIND
    }

    fn context(&self) -> Self::ModuleStateMachineContext;

    async fn handle_cli_command(
        &self,
        _args: &[ffi::OsString],
    ) -> anyhow::Result<serde_json::Value> {
        Err(anyhow::format_err!(
            "This module does not implement cli commands"
        ))
    }

    /// Returns the fee the processing of this input requires.
    ///
    /// If the semantics of a given input aren't known this function returns
    /// `None`, this only happens if a future version of Fedimint introduces a
    /// new input variant. For clients this should only be the case when
    /// processing transactions created by other users, so the result of
    /// this function can be `unwrap`ped whenever dealing with inputs
    /// generated by ourselves.
    fn input_fee(&self, input: &<Self::Common as ModuleCommon>::Input) -> Option<Amount>;

    /// Returns the fee the processing of this output requires.
    ///
    /// If the semantics of a given output aren't known this function returns
    /// `None`, this only happens if a future version of Fedimint introduces a
    /// new output variant. For clients this should only be the case when
    /// processing transactions created by other users, so the result of
    /// this function can be `unwrap`ped whenever dealing with inputs
    /// generated by ourselves.
    fn output_fee(&self, output: &<Self::Common as ModuleCommon>::Output) -> Option<Amount>;

    fn supports_backup(&self) -> bool {
        false
    }

    async fn backup(&self) -> anyhow::Result<Self::Backup> {
        anyhow::bail!("Backup not supported");
    }

    /// Does this module support being a primary module
    ///
    /// If it does it must implement:
    ///
    /// * [`Self::create_final_inputs_and_outputs`]
    /// * [`Self::await_primary_module_output`]
    /// * [`Self::get_balance`]
    /// * [`Self::subscribe_balance_changes`]
    fn supports_being_primary(&self) -> bool {
        false
    }

    /// Creates all inputs and outputs necessary to balance the transaction.
    /// The function returns an error if and only if the client's funds are not
    /// sufficient to create the inputs necessary to fully fund the transaction.
    ///
    /// A returned input also contains:
    /// * A set of private keys belonging to the input for signing the
    ///   transaction
    /// * A closure that generates states belonging to the input. This closure
    ///   takes the transaction id of the transaction in which the input was
    ///   used and the input index as input since these cannot be known at time
    ///   of calling `create_funding_input` and have to be injected later.
    ///
    /// A returned output also contains:
    /// * A closure that generates states belonging to the output. This closure
    ///   takes the transaction id of the transaction in which the output was
    ///   used and the output index as input since these cannot be known at time
    ///   of calling `create_change_output` and have to be injected later.

    async fn create_final_inputs_and_outputs(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
        _operation_id: OperationId,
        _input_amount: Amount,
        _output_amount: Amount,
    ) -> anyhow::Result<(
        Vec<ClientInput<<Self::Common as ModuleCommon>::Input, Self::States>>,
        Vec<ClientOutput<<Self::Common as ModuleCommon>::Output, Self::States>>,
    )> {
        unimplemented!()
    }

    /// Waits for the funds from an output created by
    /// [`Self::create_final_inputs_and_outputs`] to become available. This
    /// function returning typically implies a change in the output of
    /// [`Self::get_balance`].
    async fn await_primary_module_output(
        &self,
        _operation_id: OperationId,
        _out_point: OutPoint,
    ) -> anyhow::Result<Amount> {
        unimplemented!()
    }

    /// Returns the balance held by this module and available for funding
    /// transactions.
    async fn get_balance(&self, _dbtx: &mut DatabaseTransaction<'_>) -> Amount {
        unimplemented!()
    }

    /// Returns a stream that will output the updated module balance each time
    /// it changes.
    async fn subscribe_balance_changes(&self) -> BoxStream<'static, ()> {
        unimplemented!()
    }

    /// Leave the federation
    ///
    /// While technically there's nothing stopping the client from just
    /// abandoning Federation at any point by deleting all the related
    /// local data, it is useful to make sure it's safe beforehand.
    ///
    /// This call indicates the desire of the caller client code
    /// to orderly and safely leave the Federation by this module instance.
    /// The goal of the implementations is to fulfil that wish,
    /// giving prompt and informative feedback if it's not yet possible.
    ///
    /// The client module implementation should handle the request
    /// and return as fast as possible avoiding blocking for longer than
    /// necessary. This would usually involve some combination of:
    ///
    /// * recording the state of being in process of leaving the Federation to
    ///   prevent initiating new conditions that could delay its completion;
    /// * performing any fast to complete cleanup/exit logic;
    /// * initiating any time-consuming logic (e.g. canceling outstanding
    ///   contracts), as background jobs, tasks machines, etc.
    /// * checking for any conditions indicating it might not be safe to leave
    ///   at the moment.
    ///
    /// This function should return `Ok` only if from the perspective
    /// of this module instance, it is safe to delete client data and
    /// stop using it, with no further actions (like background jobs) required
    /// to complete.
    ///
    /// This function should return an error if it's not currently possible
    /// to safely (e.g. without loosing funds) leave the Federation.
    /// It should avoid running indefinitely trying to complete any cleanup
    /// actions necessary to reach a clean state, preferring spawning new
    /// state machines and returning an informative error about cleanup
    /// still in progress.
    ///
    /// If any internal task needs to complete, any user action is required,
    /// or even external condition needs to be met this function
    /// should return a `Err`.
    ///
    /// Notably modules should not disable interaction that might be necessary
    /// for the user (possibly through other modules) to leave the Federation.
    /// In particular a Mint module should retain ability to create new notes,
    /// and LN module should retain ability to send funds out.
    ///
    /// Calling code must NOT assume that a module that once returned `Ok`,
    /// will not return `Err` at later point. E.g. a Mint module might have
    /// no outstanding balance at first, but other modules winding down
    /// might "cash-out" to Ecash.
    ///
    /// Before leaving the Federation and deleting any state the calling code
    /// must collect a full round of `Ok` from all the modules.
    ///
    /// Calling code should allow the user to override and ignore any
    /// outstanding errors, after sufficient amount of warnings. Ideally,
    /// this should be done on per-module basis, to avoid mistakes.
    async fn leave(&self, _dbtx: &mut DatabaseTransaction<'_>) -> anyhow::Result<()> {
        bail!("Unable to determine if safe to leave the federation: Not implemented")
    }
}

/// Type-erased version of [`ClientModule`]
#[apply(async_trait_maybe_send!)]
pub trait IClientModule: Debug {
    fn as_any(&self) -> &(maybe_add_send_sync!(dyn std::any::Any));

    fn decoder(&self) -> Decoder;

    fn context(&self, instance: ModuleInstanceId) -> DynContext;

    async fn handle_cli_command(&self, args: &[ffi::OsString])
        -> anyhow::Result<serde_json::Value>;

    fn input_fee(&self, input: &DynInput) -> Option<Amount>;

    fn output_fee(&self, output: &DynOutput) -> Option<Amount>;

    fn supports_backup(&self) -> bool;

    async fn backup(&self, module_instance_id: ModuleInstanceId)
        -> anyhow::Result<DynModuleBackup>;

    fn supports_being_primary(&self) -> bool;

    async fn create_final_inputs_and_outputs(
        &self,
        module_instance: ModuleInstanceId,
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
        input_amount: Amount,
        output_amount: Amount,
    ) -> anyhow::Result<(Vec<ClientInput>, Vec<ClientOutput>)>;

    async fn await_primary_module_output(
        &self,
        operation_id: OperationId,
        out_point: OutPoint,
    ) -> anyhow::Result<Amount>;

    async fn get_balance(
        &self,
        module_instance: ModuleInstanceId,
        dbtx: &mut DatabaseTransaction<'_>,
    ) -> Amount;

    async fn subscribe_balance_changes(&self) -> BoxStream<'static, ()>;
}

#[apply(async_trait_maybe_send!)]
impl<T> IClientModule for T
where
    T: ClientModule,
{
    fn as_any(&self) -> &(maybe_add_send_sync!(dyn Any)) {
        self
    }

    fn decoder(&self) -> Decoder {
        T::decoder()
    }

    fn context(&self, instance: ModuleInstanceId) -> DynContext {
        DynContext::from_typed(instance, <T as ClientModule>::context(self))
    }

    async fn handle_cli_command(
        &self,
        args: &[ffi::OsString],
    ) -> anyhow::Result<serde_json::Value> {
        <T as ClientModule>::handle_cli_command(self, args).await
    }

    fn input_fee(&self, input: &DynInput) -> Option<Amount> {
        <T as ClientModule>::input_fee(
            self,
            input
                .as_any()
                .downcast_ref()
                .expect("Dispatched to correct module"),
        )
    }

    fn output_fee(&self, output: &DynOutput) -> Option<Amount> {
        <T as ClientModule>::output_fee(
            self,
            output
                .as_any()
                .downcast_ref()
                .expect("Dispatched to correct module"),
        )
    }

    fn supports_backup(&self) -> bool {
        <T as ClientModule>::supports_backup(self)
    }

    async fn backup(
        &self,
        module_instance_id: ModuleInstanceId,
    ) -> anyhow::Result<DynModuleBackup> {
        Ok(DynModuleBackup::from_typed(
            module_instance_id,
            <T as ClientModule>::backup(self).await?,
        ))
    }

    fn supports_being_primary(&self) -> bool {
        <T as ClientModule>::supports_being_primary(self)
    }

    async fn create_final_inputs_and_outputs(
        &self,
        module_instance: ModuleInstanceId,
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
        input_amount: Amount,
        output_amount: Amount,
    ) -> anyhow::Result<(Vec<ClientInput>, Vec<ClientOutput>)> {
        let (inputs, outputs) = <T as ClientModule>::create_final_inputs_and_outputs(
            self,
            &mut dbtx.to_ref_with_prefix_module_id(module_instance),
            operation_id,
            input_amount,
            output_amount,
        )
        .await?;

        let inputs = inputs
            .into_iter()
            .map(|input| input.into_dyn(module_instance))
            .collect::<Vec<ClientInput>>();

        let outputs = outputs
            .into_iter()
            .map(|output| output.into_dyn(module_instance))
            .collect::<Vec<ClientOutput>>();

        Ok((inputs, outputs))
    }

    async fn await_primary_module_output(
        &self,
        operation_id: OperationId,
        out_point: OutPoint,
    ) -> anyhow::Result<Amount> {
        <T as ClientModule>::await_primary_module_output(self, operation_id, out_point).await
    }

    async fn get_balance(
        &self,
        module_instance: ModuleInstanceId,
        dbtx: &mut DatabaseTransaction<'_>,
    ) -> Amount {
        <T as ClientModule>::get_balance(
            self,
            &mut dbtx.to_ref_with_prefix_module_id(module_instance),
        )
        .await
    }

    async fn subscribe_balance_changes(&self) -> BoxStream<'static, ()> {
        <T as ClientModule>::subscribe_balance_changes(self).await
    }
}

dyn_newtype_define!(
    #[derive(Clone)]
    pub DynClientModule(Arc<IClientModule>)
);

impl AsRef<maybe_add_send_sync!(dyn IClientModule + 'static)> for DynClientModule {
    fn as_ref(&self) -> &maybe_add_send_sync!(dyn IClientModule + 'static) {
        self.inner.as_ref()
    }
}

pub type StateGenerator<S> =
    Arc<maybe_add_send_sync!(dyn Fn(TransactionId, u64) -> Vec<S> + 'static)>;
