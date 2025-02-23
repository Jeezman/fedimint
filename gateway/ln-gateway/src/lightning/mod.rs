pub mod cln;
pub mod lnd;

use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Subcommand;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::task::TaskGroup;
use fedimint_core::util::SafeUrl;
use fedimint_core::{secp256k1, Amount};
use fedimint_ln_common::PrunedInvoice;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use self::cln::{NetworkLnRpcClient, RouteHtlcStream};
use self::lnd::GatewayLndClient;
use crate::envs::{
    FM_GATEWAY_LIGHTNING_ADDR_ENV, FM_LND_MACAROON_ENV, FM_LND_RPC_ADDR_ENV, FM_LND_TLS_CERT_ENV,
};
use crate::gateway_lnrpc::{
    CloseChannelsWithPeerResponse, CreateInvoiceRequest, CreateInvoiceResponse, EmptyResponse,
    GetFundingAddressResponse, GetNodeInfoResponse, GetRouteHintsResponse, InterceptHtlcResponse,
    PayInvoiceRequest, PayInvoiceResponse,
};

pub const MAX_LIGHTNING_RETRIES: u32 = 10;

#[derive(
    Error, Debug, Serialize, Deserialize, Encodable, Decodable, Clone, Eq, PartialEq, Hash,
)]
pub enum LightningRpcError {
    #[error("Failed to connect to Lightning node")]
    FailedToConnect,
    #[error("Failed to retrieve node info: {failure_reason}")]
    FailedToGetNodeInfo { failure_reason: String },
    #[error("Failed to retrieve route hints: {failure_reason}")]
    FailedToGetRouteHints { failure_reason: String },
    #[error("Payment failed: {failure_reason}")]
    FailedPayment { failure_reason: String },
    #[error("Failed to route HTLCs: {failure_reason}")]
    FailedToRouteHtlcs { failure_reason: String },
    #[error("Failed to complete HTLC: {failure_reason}")]
    FailedToCompleteHtlc { failure_reason: String },
    #[error("Failed to open channel: {failure_reason}")]
    FailedToOpenChannel { failure_reason: String },
    #[error("Failed to close channel: {failure_reason}")]
    FailedToCloseChannelsWithPeer { failure_reason: String },
    #[error("Failed to get Invoice: {failure_reason}")]
    FailedToGetInvoice { failure_reason: String },
    #[error("Failed to get funding address: {failure_reason}")]
    FailedToGetFundingAddress { failure_reason: String },
    #[error("Failed to connect to peer: {failure_reason}")]
    FailedToConnectToPeer { failure_reason: String },
    #[error("Failed to list active channels: {failure_reason}")]
    FailedToListActiveChannels { failure_reason: String },
    #[error("Failed to wait for chain sync: {failure_reason}")]
    FailedToWaitForChainSync { failure_reason: String },
}

/// A trait that the gateway uses to interact with a lightning node. This allows
/// the gateway to be agnostic to the specific lightning node implementation
/// being used.
#[async_trait]
pub trait ILnRpcClient: Debug + Send + Sync {
    /// Get the public key and alias of the lightning node
    async fn info(&self) -> Result<GetNodeInfoResponse, LightningRpcError>;

    /// Get route hints to the lightning node
    async fn routehints(
        &self,
        num_route_hints: usize,
    ) -> Result<GetRouteHintsResponse, LightningRpcError>;

    /// Attempt to pay an invoice using the lightning node
    async fn pay(
        &self,
        invoice: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, LightningRpcError>;

    /// Attempt to pay an invoice using the lightning node using a
    /// [`PrunedInvoice`], increasing the user's privacy by not sending the
    /// invoice description to the gateway.
    async fn pay_private(
        &self,
        _invoice: PrunedInvoice,
        _max_delay: u64,
        _max_fee: Amount,
    ) -> Result<PayInvoiceResponse, LightningRpcError> {
        Err(LightningRpcError::FailedPayment {
            failure_reason: "Private payments not supported".to_string(),
        })
    }

    /// Returns true if the lightning backend supports payments without full
    /// invoices. If this returns true, then [`ILnRpcClient::pay_private`] has
    /// to be implemented.
    fn supports_private_payments(&self) -> bool {
        false
    }

    /// Consumes the current client and returns a stream of intercepted HTLCs
    /// and a new client. `complete_htlc` must be called for all successfully
    /// intercepted HTLCs sent to the returned stream.
    ///
    /// `route_htlcs` can only be called once for a given client, since the
    /// returned stream grants exclusive routing decisions to the caller.
    /// For this reason, `route_htlc` consumes the client and returns one
    /// wrapped in an `Arc`. This lets the compiler enforce that `route_htlcs`
    /// can only be called once for a given client, since the value inside
    /// the `Arc` cannot be consumed.
    async fn route_htlcs<'a>(
        self: Box<Self>,
        task_group: &mut TaskGroup,
    ) -> Result<(RouteHtlcStream<'a>, Arc<dyn ILnRpcClient>), LightningRpcError>;

    /// Complete an HTLC that was intercepted by the gateway. Must be called for
    /// all successfully intercepted HTLCs sent to the stream returned by
    /// `route_htlcs`.
    async fn complete_htlc(
        &self,
        htlc: InterceptHtlcResponse,
    ) -> Result<EmptyResponse, LightningRpcError>;

    async fn create_invoice(
        &self,
        create_invoice_request: CreateInvoiceRequest,
    ) -> Result<CreateInvoiceResponse, LightningRpcError>;

    /// Connect to a peer lightning node from the gateway's lightning node.
    async fn connect_to_peer(
        &self,
        pubkey: secp256k1::PublicKey,
        host: String,
    ) -> Result<EmptyResponse, LightningRpcError>;

    /// Get a funding address belonging to the gateway's lightning node
    /// wallet.
    async fn get_funding_address(&self) -> Result<GetFundingAddressResponse, LightningRpcError>;

    /// Open a channel with a peer lightning node from the gateway's lightning
    /// node.
    async fn open_channel(
        &self,
        pubkey: secp256k1::PublicKey,
        channel_size_sats: u64,
        push_amount_sats: u64,
    ) -> Result<EmptyResponse, LightningRpcError>;

    /// Close all channels with a peer lightning node from the gateway's
    /// lightning node.
    async fn close_channels_with_peer(
        &self,
        pubkey: secp256k1::PublicKey,
    ) -> Result<CloseChannelsWithPeerResponse, LightningRpcError>;

    async fn list_active_channels(&self) -> Result<Vec<ChannelInfo>, LightningRpcError>;
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChannelInfo {
    pub remote_pubkey: String,
    pub channel_size_sats: u64,
    pub outbound_liquidity_sats: u64,
    pub inbound_liquidity_sats: u64,
    pub short_channel_id: u64,
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum LightningMode {
    #[clap(name = "lnd")]
    Lnd {
        /// LND RPC address
        #[arg(long = "lnd-rpc-host", env = FM_LND_RPC_ADDR_ENV)]
        lnd_rpc_addr: String,

        /// LND TLS cert file path
        #[arg(long = "lnd-tls-cert", env = FM_LND_TLS_CERT_ENV)]
        lnd_tls_cert: String,

        /// LND macaroon file path
        #[arg(long = "lnd-macaroon", env = FM_LND_MACAROON_ENV)]
        lnd_macaroon: String,
    },
    #[clap(name = "cln")]
    Cln {
        #[arg(long = "cln-extension-addr", env = FM_GATEWAY_LIGHTNING_ADDR_ENV)]
        cln_extension_addr: SafeUrl,
    },
}

#[async_trait]
pub trait LightningBuilder {
    async fn build(&self) -> Box<dyn ILnRpcClient>;
}

#[derive(Clone)]
pub struct GatewayLightningBuilder {
    pub lightning_mode: LightningMode,
}

#[async_trait]
impl LightningBuilder for GatewayLightningBuilder {
    async fn build(&self) -> Box<dyn ILnRpcClient> {
        match self.lightning_mode.clone() {
            LightningMode::Cln { cln_extension_addr } => {
                Box::new(NetworkLnRpcClient::new(cln_extension_addr))
            }
            LightningMode::Lnd {
                lnd_rpc_addr,
                lnd_tls_cert,
                lnd_macaroon,
            } => Box::new(GatewayLndClient::new(
                lnd_rpc_addr,
                lnd_tls_cert,
                lnd_macaroon,
                None,
            )),
        }
    }
}
