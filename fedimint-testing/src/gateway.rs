use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use fedimint_client::module::init::ClientModuleInitRegistry;
use fedimint_client::ClientHandleArc;
use fedimint_core::config::FederationId;
use fedimint_core::db::mem_impl::MemDatabase;
use fedimint_core::db::Database;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::task::{block_in_place, block_on, sleep_in_test, TaskGroup};
use fedimint_core::util::SafeUrl;
use fedimint_logging::LOG_TEST;
use lightning_invoice::RoutingFees;
use ln_gateway::client::GatewayClientBuilder;
use ln_gateway::lightning::{ILnRpcClient, LightningBuilder};
use ln_gateway::rpc::rpc_client::GatewayRpcClient;
use ln_gateway::rpc::{ConnectFedPayload, FederationInfo, V1_API_ENDPOINT};
use ln_gateway::{Gateway, GatewayState};
use tracing::{info, warn};

use crate::federation::FederationTest;
use crate::fixtures::test_dir;
use crate::ln::FakeLightningTest;

pub const DEFAULT_GATEWAY_PASSWORD: &str = "thereisnosecondbest";

/// Fixture for creating a gateway
pub struct GatewayTest {
    /// URL for the RPC
    pub versioned_api: SafeUrl,
    /// Handle of the running gateway
    pub gateway: Gateway,
    // Public key of the lightning node
    pub node_pub_key: PublicKey,
    // Listening address of the lightning node
    pub listening_addr: String,
    /// `TaskGroup` that is running the test
    task_group: TaskGroup,
}

impl GatewayTest {
    /// RPC client for communicating with the gateway admin API
    pub fn get_rpc(&self) -> GatewayRpcClient {
        GatewayRpcClient::new(self.versioned_api.clone(), None)
    }

    pub async fn select_client(&self, federation_id: FederationId) -> ClientHandleArc {
        self.gateway
            .select_client(federation_id)
            .await
            .unwrap()
            .into_value()
    }

    /// Connects to a new federation and stores the info
    pub async fn connect_fed(&mut self, fed: &FederationTest) -> FederationInfo {
        info!(target: LOG_TEST, "Sending rpc to connect gateway to federation");
        let invite_code = fed.invite_code().to_string();
        let rpc = self
            .get_rpc()
            .with_password(Some(DEFAULT_GATEWAY_PASSWORD.to_string()));
        rpc.connect_federation(ConnectFedPayload { invite_code })
            .await
            .unwrap()
    }

    pub fn get_gateway_id(&self) -> PublicKey {
        self.gateway.gateway_id
    }

    pub(crate) async fn new(
        base_port: u16,
        cli_password: Option<String>,
        lightning: FakeLightningTest,
        decoders: ModuleDecoderRegistry,
        registry: ClientModuleInitRegistry,
        num_route_hints: u32,
    ) -> Self {
        let listen: SocketAddr = format!("127.0.0.1:{base_port}").parse().unwrap();
        let address: SafeUrl = format!("http://{listen}").parse().unwrap();
        let versioned_api = address.join(V1_API_ENDPOINT).unwrap();

        let (path, _config_dir) = test_dir(&format!("gateway-{}", rand::random::<u64>()));

        // Create federation client builder for the gateway
        let client_builder: GatewayClientBuilder =
            GatewayClientBuilder::new(path.clone(), registry, 0);

        let lightning_builder: Arc<dyn LightningBuilder + Send + Sync> =
            Arc::new(FakeLightningBuilder);

        let gateway_db = Database::new(MemDatabase::new(), decoders.clone());

        let gateway = Gateway::new_with_custom_registry(
            lightning_builder,
            client_builder,
            listen,
            address.clone(),
            cli_password.clone(),
            None, // Use default Network which is "regtest"
            RoutingFees {
                base_msat: 0,
                proportional_millionths: 0,
            },
            num_route_hints,
            gateway_db,
        )
        .await
        .expect("Failed to create gateway");

        let gateway_run = gateway.clone();
        let root_group = TaskGroup::new();
        let mut tg = root_group.clone();
        root_group.spawn("Gateway Run", |_handle| async move {
            gateway_run
                .run(&mut tg)
                .await
                .expect("Failed to start gateway");
        });

        // Wait for the gateway web server to be available
        GatewayTest::wait_for_webserver(versioned_api.clone(), cli_password)
            .await
            .expect("Gateway web server failed to start");

        // Wait for the gateway to be in the configuring or running state
        GatewayTest::wait_for_gateway_state(gateway.clone(), |gw_state| {
            matches!(gw_state, GatewayState::Configuring)
                || matches!(gw_state, GatewayState::Running { .. })
        })
        .await
        .expect("Gateway failed to start");

        let listening_addr = lightning.listening_address();
        let info = lightning.info().await.unwrap();

        Self {
            versioned_api,
            gateway,
            node_pub_key: PublicKey::from_slice(info.pub_key.as_slice()).unwrap(),
            listening_addr,
            task_group: root_group,
        }
    }

    /// Waits for the webserver to be ready.
    ///
    /// This function is used to ensure that the webserver is fully initialized
    /// and ready to accept incoming requests. It is designed to be used in
    /// a concurrent environment where the webserver might be initialized in a
    /// separate thread or task.
    pub async fn wait_for_webserver(
        versioned_api: SafeUrl,
        password: Option<String>,
    ) -> anyhow::Result<()> {
        let rpc = GatewayRpcClient::new(versioned_api, password);
        for _ in 0..30 {
            let rpc_result = rpc.get_info().await;
            if rpc_result.is_ok() {
                return Ok(());
            }

            sleep_in_test("waiting for webserver to be ready", Duration::from_secs(1)).await;
        }

        Err(anyhow!(
            "Gateway web server did not come up within 30 seconds"
        ))
    }

    pub async fn wait_for_gateway_state(
        gateway: Gateway,
        func: impl Fn(GatewayState) -> bool,
    ) -> anyhow::Result<()> {
        for _ in 0..30 {
            let gw_state = gateway.state.read().await.clone();
            if func(gw_state) {
                return Ok(());
            }

            sleep_in_test("waiting for gateway state", Duration::from_secs(1)).await;
        }

        Err(anyhow!(
            "Gateway did not reach desired state within 30 seconds"
        ))
    }
}

impl Drop for GatewayTest {
    fn drop(&mut self) {
        block_in_place(move || {
            block_on(async move {
                if let Err(e) = self.task_group.clone().shutdown_join_all(None).await {
                    warn!("Got error shutting down GatewayTest: {e:?}");
                }
            });
        });
    }
}
#[derive(Debug, Clone)]
pub enum LightningNodeType {
    Cln,
    Lnd,
}

impl Display for LightningNodeType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        match self {
            LightningNodeType::Cln => write!(f, "cln"),
            LightningNodeType::Lnd => write!(f, "lnd"),
        }
    }
}

impl FromStr for LightningNodeType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "cln" => Ok(LightningNodeType::Cln),
            "lnd" => Ok(LightningNodeType::Lnd),
            _ => Err(format!("Invalid value for LightningNodeType: {s}")),
        }
    }
}

#[derive(Clone)]
pub struct FakeLightningBuilder;

#[async_trait]
impl LightningBuilder for FakeLightningBuilder {
    async fn build(&self) -> Box<dyn ILnRpcClient> {
        Box::new(FakeLightningTest::new())
    }
}
