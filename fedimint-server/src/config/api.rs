use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bitcoin_hashes::sha256;
use fedimint_api_client::api::{DynGlobalApi, StatusResponse};
use fedimint_core::admin_client::{
    ConfigGenConnectionsRequest, ConfigGenParamsConsensus, ConfigGenParamsRequest,
    ConfigGenParamsResponse, PeerServerParams, ServerStatus,
};
use fedimint_core::config::{
    ConfigGenModuleParams, ServerModuleConfigGenParamsRegistry, ServerModuleInitRegistry,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::Database;
use fedimint_core::encoding::Encodable;
use fedimint_core::endpoint_constants::{
    ADD_CONFIG_GEN_PEER_ENDPOINT, AUTH_ENDPOINT, CONFIG_GEN_PEERS_ENDPOINT,
    CONSENSUS_CONFIG_GEN_PARAMS_ENDPOINT, DEFAULT_CONFIG_GEN_PARAMS_ENDPOINT,
    RESTART_FEDERATION_SETUP_ENDPOINT, RUN_DKG_ENDPOINT, SET_CONFIG_GEN_CONNECTIONS_ENDPOINT,
    SET_CONFIG_GEN_PARAMS_ENDPOINT, SET_PASSWORD_ENDPOINT, START_CONSENSUS_ENDPOINT,
    STATUS_ENDPOINT, VERIFIED_CONFIGS_ENDPOINT, VERIFY_CONFIG_HASH_ENDPOINT,
};
use fedimint_core::module::{
    api_endpoint, ApiAuth, ApiEndpoint, ApiEndpointContext, ApiError, ApiRequestErased, ApiVersion,
};
use fedimint_core::task::{sleep, TaskGroup};
use fedimint_core::util::SafeUrl;
use fedimint_core::PeerId;
use itertools::Itertools;
use tokio::sync::mpsc::Sender;
use tokio::sync::{Mutex, MutexGuard};
use tokio_rustls::rustls;
use tracing::{error, info};

use crate::config::{gen_cert_and_key, ConfigGenParams, ServerConfig};
use crate::envs::FM_PEER_ID_SORT_BY_URL_ENV;
use crate::net::api::{check_auth, ApiResult, HasApiContext};
use crate::net::peers::DelayCalculator;

/// Serves the config gen API endpoints
#[derive(Clone)]
pub struct ConfigGenApi {
    /// In-memory state machine
    state: Arc<Mutex<ConfigGenState>>,
    /// DB not really used
    db: Database,
    /// Tracks when the config is generated
    config_generated_tx: Sender<ServerConfig>,
    /// Task group for running DKG
    task_group: TaskGroup,
    /// Code version str that will get encoded in consensus hash
    code_version_str: String,
    /// Api secret to use
    api_secret: Option<String>,
}

impl ConfigGenApi {
    pub fn new(
        settings: ConfigGenSettings,
        db: Database,
        config_generated_tx: Sender<ServerConfig>,
        task_group: &mut TaskGroup,
        code_version_str: String,
        api_secret: Option<String>,
    ) -> Self {
        let config_gen_api = Self {
            state: Arc::new(Mutex::new(ConfigGenState::new(settings))),
            db,
            config_generated_tx,
            task_group: task_group.clone(),
            code_version_str,
            api_secret,
        };
        info!(target: fedimint_logging::LOG_NET_PEER_DKG, "Created new config gen Api");
        config_gen_api
    }

    // Sets the auth and decryption key derived from the password
    pub async fn set_password(&self, auth: ApiAuth) -> ApiResult<()> {
        let mut state = self.require_status(ServerStatus::AwaitingPassword).await?;
        state.auth = Some(auth);
        state.status = ServerStatus::SharingConfigGenParams;
        info!(
            target: fedimint_logging::LOG_NET_PEER_DKG,
            "Set password for config gen"
        );
        Ok(())
    }

    async fn require_status(&self, status: ServerStatus) -> ApiResult<MutexGuard<ConfigGenState>> {
        let state = self.state.lock().await;
        if state.status != status {
            return Self::bad_request(&format!("Expected to be in {status:?} state"));
        }
        Ok(state)
    }

    async fn require_any_status(
        &self,
        statuses: &[ServerStatus],
    ) -> ApiResult<MutexGuard<ConfigGenState>> {
        let state = self.state.lock().await;
        if !statuses.contains(&state.status) {
            return Self::bad_request(&format!("Expected to be in one of {statuses:?} states"));
        }
        Ok(state)
    }

    /// Sets our connection info, possibly sending it to a leader
    pub async fn set_config_gen_connections(
        &self,
        request: ConfigGenConnectionsRequest,
    ) -> ApiResult<()> {
        {
            let mut state = self
                .require_status(ServerStatus::SharingConfigGenParams)
                .await?;
            state.set_request(request)?;
        }
        self.update_leader().await?;
        Ok(())
    }

    /// Sends our updated peer info to the leader (if we have one)
    async fn update_leader(&self) -> ApiResult<()> {
        let state = self.state.lock().await.clone();
        let local = state.local.clone();

        if let Some(url) = local.and_then(|local| local.leader_api_url) {
            DynGlobalApi::from_pre_peer_id_admin_endpoint(url, &self.api_secret)
                .add_config_gen_peer(state.our_peer_info()?)
                .await
                .map_err(|_| ApiError::not_found("Unable to connect to the leader".to_string()))?;
        }
        Ok(())
    }

    /// Called from `set_config_gen_connections` to add a peer's connection info
    /// to the leader
    pub async fn add_config_gen_peer(&self, peer: PeerServerParams) -> ApiResult<()> {
        let mut state = self.state.lock().await;
        state.peers.insert(peer.api_url.clone(), peer);
        info!(target: fedimint_logging::LOG_NET_PEER_DKG, "New peer added to config gen");
        Ok(())
    }

    /// Returns the peers that have called `add_config_gen_peer` on the leader
    pub async fn config_gen_peers(&self) -> ApiResult<Vec<PeerServerParams>> {
        let state = self.state.lock().await;
        Ok(state.get_peer_info().into_values().collect())
    }

    /// Returns default config gen params that can be modified by the leader
    pub async fn default_config_gen_params(&self) -> ApiResult<ConfigGenParamsRequest> {
        let state = self.state.lock().await;
        Ok(state.settings.default_params.clone())
    }

    /// Sets and validates the config gen params
    ///
    /// The leader passes consensus params, everyone passes local params
    pub async fn set_config_gen_params(&self, request: ConfigGenParamsRequest) -> ApiResult<()> {
        self.consensus_config_gen_params(&request).await?;
        let mut state = self
            .require_status(ServerStatus::SharingConfigGenParams)
            .await?;
        state.requested_params = Some(request);
        info!(
            target: fedimint_logging::LOG_NET_PEER_DKG,
            "Set params for config gen"
        );
        Ok(())
    }

    async fn get_requested_params(&self) -> ApiResult<ConfigGenParamsRequest> {
        let state = self.state.lock().await.clone();
        state.requested_params.ok_or(ApiError::bad_request(
            "Config params were not set on this guardian".to_string(),
        ))
    }

    /// Gets the consensus config gen params
    pub async fn consensus_config_gen_params(
        &self,
        request: &ConfigGenParamsRequest,
    ) -> ApiResult<ConfigGenParamsResponse> {
        let state = self.state.lock().await.clone();
        let local = state.local.clone();

        let consensus = match local.and_then(|local| local.leader_api_url) {
            Some(leader_url) => {
                let client = DynGlobalApi::from_pre_peer_id_admin_endpoint(
                    leader_url.clone(),
                    &self.api_secret,
                );
                let response = client.consensus_config_gen_params().await;
                response
                    .map_err(|_| ApiError::not_found("Cannot get leader params".to_string()))?
                    .consensus
            }
            None => ConfigGenParamsConsensus {
                peers: state.get_peer_info(),
                meta: request.meta.clone(),
                modules: request.modules.clone(),
            },
        };

        let params = state.get_config_gen_params(request, consensus.clone())?;
        Ok(ConfigGenParamsResponse {
            consensus,
            our_current_id: params.local.our_id,
        })
    }

    /// Once configs are generated, updates status to ReadyForConfigGen and
    /// spawns a task to coordinate DKG, then returns. Coordinating DKG in a
    /// separate thread allows clients to poll the server status instead of
    /// blocking until completion, which can be fragile due to timeouts, poor
    /// network connections, etc.
    ///
    /// Calling a second time will return an error.
    pub async fn run_dkg(&self) -> ApiResult<()> {
        let leader = {
            let mut state = self
                .require_status(ServerStatus::SharingConfigGenParams)
                .await?;
            // Update our state
            state.status = ServerStatus::ReadyForConfigGen;
            info!(
                target: fedimint_logging::LOG_NET_PEER_DKG,
                "Update config gen status to 'Ready for config gen'"
            );
            // Create a WSClient for the leader
            state.local.clone().and_then(|local| {
                local.leader_api_url.map(|url| {
                    DynGlobalApi::from_pre_peer_id_admin_endpoint(url, &self.api_secret.clone())
                })
            })
        };

        self.update_leader().await?;

        let self_clone = self.clone();
        let sub_group = self.task_group.make_subgroup();
        sub_group.spawn("run dkg", move |_handle| async move {
            // Followers wait for leader to signal readiness for DKG
            if let Some(client) = leader {
                loop {
                    let status = client.status().await.map_err(|_| {
                        ApiError::not_found("Unable to connect to the leader".to_string())
                    })?;
                    if status.server == ServerStatus::ReadyForConfigGen {
                        break;
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            };

            // Get params and registry
            let request = self_clone.get_requested_params().await?;
            let response = self_clone.consensus_config_gen_params(&request).await?;
            let (params, registry) = {
                let state: MutexGuard<'_, ConfigGenState> = self_clone
                    .require_status(ServerStatus::ReadyForConfigGen)
                    .await?;
                let params = state.get_config_gen_params(&request, response.consensus)?;
                let registry = state.settings.registry.clone();
                (params, registry)
            };

            // Run DKG
            let mut task_group: TaskGroup = self_clone.task_group.make_subgroup();
            let config = ServerConfig::distributed_gen(
                &params,
                registry,
                DelayCalculator::PROD_DEFAULT,
                &mut task_group,
                self_clone.code_version_str.clone(),
            )
            .await;
            task_group
                .shutdown_join_all(None)
                .await
                .expect("shuts down");

            {
                let mut state = self_clone.state.lock().await;
                match config {
                    Ok(config) => {
                        state.status = ServerStatus::VerifyingConfigs;
                        state.config = Some(config);
                        info!(
                            target: fedimint_logging::LOG_NET_PEER_DKG,
                            "Set config for config gen"
                        );
                    }
                    Err(e) => {
                        error!(
                            target: fedimint_logging::LOG_NET_PEER_DKG,
                            "DKG failed with {:?}", e
                        );
                        state.status = ServerStatus::ConfigGenFailed;
                        info!(
                            target: fedimint_logging::LOG_NET_PEER_DKG,
                            "Update config gen status to 'Config gen failed'"
                        );
                    }
                }
            }
            self_clone.update_leader().await
        });

        Ok(())
    }

    /// Returns tagged hashes of consensus config to be shared with other peers.
    /// The hashes are tagged with the peer id  such that they are unique to
    /// each peer and their manual verification by the guardians via the UI is
    /// more robust.
    pub async fn verify_config_hash(&self) -> ApiResult<BTreeMap<PeerId, sha256::Hash>> {
        let expected_status = [
            ServerStatus::VerifyingConfigs,
            ServerStatus::VerifiedConfigs,
        ];

        let state = self.require_any_status(&expected_status).await?;

        let config = state
            .config
            .clone()
            .ok_or(ApiError::bad_request("Missing config".to_string()))?;

        let verification_hashes = config
            .consensus
            .api_endpoints
            .keys()
            .map(|peer| (*peer, (*peer, config.consensus.clone()).consensus_hash()))
            .collect();

        Ok(verification_hashes)
    }

    /// We have verified all our peer configs
    pub async fn verified_configs(&self) -> ApiResult<()> {
        {
            let expected_status = [
                ServerStatus::VerifyingConfigs,
                ServerStatus::VerifiedConfigs,
            ];
            let mut state = self.require_any_status(&expected_status).await?;
            if state.status == ServerStatus::VerifiedConfigs {
                return Ok(());
            }
            state.status = ServerStatus::VerifiedConfigs;
            info!(
                target: fedimint_logging::LOG_NET_PEER_DKG,
                "Update config gen status to 'Verified configs'"
            );
        }

        self.update_leader().await?;
        Ok(())
    }

    pub async fn start_consensus(&self) -> ApiResult<()> {
        let state = self
            .require_any_status(&[
                ServerStatus::VerifyingConfigs,
                ServerStatus::VerifiedConfigs,
            ])
            .await?;

        self.config_generated_tx
            .send(state.config.clone().expect("Config should exist"))
            .await
            .expect("Can send");

        Ok(())
    }

    /// Returns the server status
    pub async fn server_status(&self) -> ServerStatus {
        self.state.lock().await.status.clone()
    }

    fn bad_request<T>(msg: &str) -> ApiResult<T> {
        Err(ApiError::bad_request(msg.to_string()))
    }

    pub async fn restart_federation_setup(&self) -> ApiResult<()> {
        let leader = {
            let expected_status = [
                ServerStatus::SharingConfigGenParams,
                ServerStatus::ReadyForConfigGen,
                ServerStatus::ConfigGenFailed,
                ServerStatus::VerifyingConfigs,
                ServerStatus::VerifiedConfigs,
            ];
            let mut state = self.require_any_status(&expected_status).await?;

            state.status = ServerStatus::SetupRestarted;
            info!(
                target: fedimint_logging::LOG_NET_PEER_DKG,
                "Update config gen status to 'Setup restarted'"
            );
            // Create a WSClient for the leader
            state.local.clone().and_then(|local| {
                local
                    .leader_api_url
                    .map(|url| DynGlobalApi::from_pre_peer_id_admin_endpoint(url, &self.api_secret))
            })
        };

        self.update_leader().await?;

        // Followers wait for leader to signal that all peers have restarted setup
        // The leader will signal this by setting it's status to AwaitingPassword
        let self_clone = self.clone();
        let sub_group = self.task_group.make_subgroup();
        sub_group.spawn("restart", move |_handle| async move {
            if let Some(client) = leader {
                self_clone.await_leader_restart(&client).await?;
            } else {
                self_clone.await_peer_restart().await;
            }
            // Progress status to AwaitingPassword
            {
                let mut state = self_clone.state.lock().await;
                state.reset();
            }
            self_clone.update_leader().await
        });

        Ok(())
    }

    // Followers wait for leader to signal that all peers have restarted setup
    async fn await_leader_restart(&self, client: &DynGlobalApi) -> ApiResult<()> {
        let mut retries = 0;
        loop {
            if let Ok(status) = client.status().await {
                if status.server == ServerStatus::AwaitingPassword
                    || status.server == ServerStatus::SharingConfigGenParams
                {
                    break Ok(());
                }
            } else {
                if retries > 3 {
                    return Err(ApiError::not_found(
                        "Unable to connect to the leader".to_string(),
                    ));
                }
                retries += 1;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    // Leader waits for all peers to restart setup,
    async fn await_peer_restart(&self) {
        loop {
            {
                let state = self.state.lock().await;
                let peers = state.peers.clone();
                if peers
                    .values()
                    .all(|peer| peer.status == Some(ServerStatus::SetupRestarted))
                {
                    break;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Config gen params that are only used locally, shouldn't be shared
#[derive(Debug, Clone)]
pub struct ConfigGenParamsLocal {
    /// Our peer id
    pub our_id: PeerId,
    /// Our TLS private key
    pub our_private_key: rustls::PrivateKey,
    /// Secret API auth string
    pub api_auth: ApiAuth,
    /// Bind address for P2P communication
    pub p2p_bind: SocketAddr,
    /// Bind address for API communication
    pub api_bind: SocketAddr,
    /// How many API connections we will accept
    pub max_connections: u32,
}

/// All the info we configure prior to config gen starting
#[derive(Debug, Clone)]
pub struct ConfigGenSettings {
    /// Limit on the number of times a config download token can be used
    pub download_token_limit: Option<u64>,
    /// Bind address for our P2P connection
    pub p2p_bind: SocketAddr,
    /// Bind address for our API connection
    pub api_bind: SocketAddr,
    /// URL for our P2P connection
    pub p2p_url: SafeUrl,
    /// URL for our API connection
    pub api_url: SafeUrl,
    /// The default params for the modules
    pub default_params: ConfigGenParamsRequest,
    /// How many API connections we will accept
    pub max_connections: u32,
    /// Registry for config gen
    pub registry: ServerModuleInitRegistry,
}

/// State held by the API after receiving a `ConfigGenConnectionsRequest`
#[derive(Debug, Clone)]
pub struct ConfigGenState {
    /// Our config gen settings configured locally
    settings: ConfigGenSettings,
    /// Our auth string
    auth: Option<ApiAuth>,
    /// Our local connection
    local: Option<ConfigGenLocalConnection>,
    /// Connection info received from other guardians, unique by api_url
    /// (because it's non-user configurable)
    peers: BTreeMap<SafeUrl, PeerServerParams>,
    /// The config gen params requested by the leader
    requested_params: Option<ConfigGenParamsRequest>,
    /// Our status
    status: ServerStatus,
    /// Configs that have been generated
    config: Option<ServerConfig>,
}

/// Our local connection info
#[derive(Debug, Clone)]
struct ConfigGenLocalConnection {
    /// Our TLS private key
    tls_private: rustls::PrivateKey,
    /// Our TLS public cert
    tls_cert: rustls::Certificate,
    /// Our guardian name
    our_name: String,
    /// URL of "leader" guardian to send our connection info to
    /// Will be `None` if we are the leader
    leader_api_url: Option<SafeUrl>,
}

impl ConfigGenState {
    fn new(settings: ConfigGenSettings) -> Self {
        Self {
            settings,
            auth: None,
            local: None,
            peers: Default::default(),
            requested_params: None,
            status: ServerStatus::AwaitingPassword,
            config: None,
        }
    }

    fn set_request(&mut self, request: ConfigGenConnectionsRequest) -> ApiResult<()> {
        let (tls_cert, tls_private) = gen_cert_and_key(&request.our_name)
            .map_err(|_| ApiError::server_error("Unable to generate TLS keys".to_string()))?;
        self.local = Some(ConfigGenLocalConnection {
            tls_private,
            tls_cert,
            our_name: request.our_name,
            leader_api_url: request.leader_api_url,
        });
        info!(
            target: fedimint_logging::LOG_NET_PEER_DKG,
            "Set local connection for config gen"
        );
        Ok(())
    }

    fn local_connection(&self) -> ApiResult<ConfigGenLocalConnection> {
        self.local.clone().ok_or(ApiError::bad_request(
            "Our connection info not set yet".to_string(),
        ))
    }

    fn auth(&self) -> ApiResult<ApiAuth> {
        self.auth
            .clone()
            .ok_or(ApiError::bad_request("Missing auth".to_string()))
    }

    fn our_peer_info(&self) -> ApiResult<PeerServerParams> {
        let local = self.local_connection()?;
        Ok(PeerServerParams {
            cert: local.tls_cert.clone(),
            p2p_url: self.settings.p2p_url.clone(),
            api_url: self.settings.api_url.clone(),
            name: local.our_name,
            status: Some(self.status.clone()),
        })
    }

    fn get_peer_info(&self) -> BTreeMap<PeerId, PeerServerParams> {
        self.peers
            .values()
            .cloned()
            .chain(self.our_peer_info().ok())
            // Since sort order here is arbitrary, try to sort by nick-names first for more natural
            // 'name -> id' mapping, which is helpful when operating on 'peer-ids' (debugging etc.);
            // Ties are OK (to_lowercase), not important in practice.
            .sorted_by_cached_key(|peer| {
                // in certain (very obscure) cases, it might be worthwhile to sort by urls, so
                // just expose it as an env var; probably no need to document it too much
                if std::env::var_os(FM_PEER_ID_SORT_BY_URL_ENV).is_some_and(|var| !var.is_empty()) {
                    peer.api_url.to_string()
                } else {
                    peer.name.to_lowercase()
                }
            })
            .enumerate()
            .map(|(i, peer)| (PeerId::from(i as u16), peer))
            .collect()
    }

    /// Validates and returns the params using our `request` and `consensus`
    /// which comes from the leader
    fn get_config_gen_params(
        &self,
        request: &ConfigGenParamsRequest,
        mut consensus: ConfigGenParamsConsensus,
    ) -> ApiResult<ConfigGenParams> {
        let local_connection = self.local_connection()?;

        let (our_id, _) = consensus
            .peers
            .iter()
            .find(|(_, param)| local_connection.tls_cert == param.cert)
            .ok_or(ApiError::bad_request(
                "Our TLS cert not found among peers".to_string(),
            ))?;

        let mut combined_params = vec![];
        let default_params = self.settings.default_params.modules.clone();
        let local_params = request.modules.clone();
        let consensus_params = consensus.modules.clone();
        // Use defaults in case local or consensus params are missing
        for (id, kind, default) in default_params.iter_modules() {
            let consensus = &consensus_params.get(id).unwrap_or(default).consensus;
            let local = &local_params.get(id).unwrap_or(default).local;
            let combined = ConfigGenModuleParams::new(local.clone(), consensus.clone());
            // Check that the params are parseable
            let module = self.settings.registry.get(kind).expect("Module exists");
            module.validate_params(&combined).map_err(|e| {
                ApiError::bad_request(format!(
                    "Module {} params invalid: {}",
                    id,
                    itertools::join(e.chain(), ": ")
                ))
            })?;
            combined_params.push((id, kind.clone(), combined));
        }
        consensus.modules = ServerModuleConfigGenParamsRegistry::from_iter(combined_params);

        let local = ConfigGenParamsLocal {
            our_id: *our_id,
            our_private_key: local_connection.tls_private,
            api_auth: self.auth()?,
            p2p_bind: self.settings.p2p_bind,
            api_bind: self.settings.api_bind,
            max_connections: self.settings.max_connections,
        };

        Ok(ConfigGenParams { local, consensus })
    }

    fn reset(&mut self) {
        self.config = None;
        self.peers = Default::default();
        self.auth = None;
        self.requested_params = None;
        self.status = ServerStatus::AwaitingPassword;
        self.local = None;

        info!(
            target: fedimint_logging::LOG_NET_PEER_DKG,
            "Reset config gen state"
        );
    }
}

#[async_trait]
impl HasApiContext<ConfigGenApi> for ConfigGenApi {
    async fn context(
        &self,
        request: &ApiRequestErased,
        id: Option<ModuleInstanceId>,
    ) -> (&ConfigGenApi, ApiEndpointContext<'_>) {
        let mut db = self.db.clone();
        let mut dbtx = self.db.begin_transaction().await;
        if let Some(id) = id {
            db = self.db.with_prefix_module_id(id);
            dbtx = dbtx.with_prefix_module_id(id);
        }
        let state = self.state.lock().await;
        let auth = request.auth.as_ref();
        let has_auth = match state.auth.clone() {
            // The first client to connect gets the set the password
            None => true,
            Some(configured_auth) => Some(&configured_auth) == auth,
        };

        (
            self,
            ApiEndpointContext::new(db, dbtx, has_auth, request.auth.clone()),
        )
    }
}

pub fn server_endpoints() -> Vec<ApiEndpoint<ConfigGenApi>> {
    vec![
        api_endpoint! {
            SET_PASSWORD_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, context, _v: ()| -> () {
                match context.request_auth() {
                    None => return Err(ApiError::bad_request("Missing password".to_string())),
                    Some(auth) => config.set_password(auth).await
                }
            }
        },
        api_endpoint! {
            SET_CONFIG_GEN_CONNECTIONS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, context, server: ConfigGenConnectionsRequest| -> () {
                check_auth(context)?;
                config.set_config_gen_connections(server).await
            }
        },
        api_endpoint! {
            ADD_CONFIG_GEN_PEER_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, _context, peer: PeerServerParams| -> () {
                // No auth required since this is an API-to-API call and the peer connections will be manually accepted or not in the UI
                config.add_config_gen_peer(peer).await
            }
        },
        api_endpoint! {
            CONFIG_GEN_PEERS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, _context, _v: ()| -> Vec<PeerServerParams> {
                config.config_gen_peers().await
            }
        },
        api_endpoint! {
            DEFAULT_CONFIG_GEN_PARAMS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, context,  _v: ()| -> ConfigGenParamsRequest {
                check_auth(context)?;
                config.default_config_gen_params().await
            }
        },
        api_endpoint! {
            SET_CONFIG_GEN_PARAMS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, context, params: ConfigGenParamsRequest| -> () {
                check_auth(context)?;
                config.set_config_gen_params(params).await
            }
        },
        api_endpoint! {
            CONSENSUS_CONFIG_GEN_PARAMS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, _context, _v: ()| -> ConfigGenParamsResponse {
                let request = config.get_requested_params().await?;
                config.consensus_config_gen_params(&request).await
            }
        },
        api_endpoint! {
            RUN_DKG_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, context, _v: ()| -> () {
                check_auth(context)?;
                config.run_dkg().await
            }
        },
        api_endpoint! {
            VERIFY_CONFIG_HASH_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, context, _v: ()| -> BTreeMap<PeerId, sha256::Hash> {
                check_auth(context)?;
                config.verify_config_hash().await
            }
        },
        api_endpoint! {
            VERIFIED_CONFIGS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, context, _v: ()| -> () {
                check_auth(context)?;
                config.verified_configs().await
            }
        },
        api_endpoint! {
            START_CONSENSUS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, context, _v: ()| -> () {
                check_auth(context)?;
                config.start_consensus().await
            }
        },
        api_endpoint! {
            STATUS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, _context, _v: ()| -> StatusResponse {
                let server = config.server_status().await;
                Ok(StatusResponse {
                    server,
                    federation: None
                })
            }
        },
        api_endpoint! {
            AUTH_ENDPOINT,
            ApiVersion::new(0, 0),
            async |_config: &ConfigGenApi, context, _v: ()| -> () {
                check_auth(context)?;
                Ok(())
            }
        },
        api_endpoint! {
            RESTART_FEDERATION_SETUP_ENDPOINT,
            ApiVersion::new(0, 0),
            async |config: &ConfigGenApi, context, _v: ()| -> () {
                check_auth(context)?;
                config.restart_federation_setup().await
            }
        },
    ]
}

#[cfg(test)]
mod tests {

    use std::collections::{BTreeMap, BTreeSet, HashSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use fedimint_api_client::api::{DynGlobalApi, FederationResult, StatusResponse};
    use fedimint_core::admin_client::{ConfigGenParamsRequest, ServerStatus};
    use fedimint_core::config::{ServerModuleConfigGenParamsRegistry, ServerModuleInitRegistry};
    use fedimint_core::db::mem_impl::MemDatabase;
    use fedimint_core::db::IRawDatabaseExt;
    use fedimint_core::module::ApiAuth;
    use fedimint_core::runtime::spawn;
    use fedimint_core::task::{sleep, TaskGroup};
    use fedimint_core::util::SafeUrl;
    use fedimint_core::Amount;
    use fedimint_dummy_common::config::{
        DummyConfig, DummyGenParams, DummyGenParamsConsensus, DummyGenParamsLocal,
    };
    use fedimint_dummy_server::DummyInit;
    use fedimint_logging::TracingSetup;
    use fedimint_portalloc::port_alloc;
    use fedimint_testing::fixtures::test_dir;
    use futures::future::join_all;
    use itertools::Itertools;
    use tracing::info;

    use crate::config::api::{ConfigGenConnectionsRequest, ConfigGenSettings};
    use crate::config::io::{read_server_config, PLAINTEXT_PASSWORD};
    use crate::config::{DynServerModuleInit, ServerConfig, DEFAULT_MAX_CLIENT_CONNECTIONS};
    use crate::fedimint_core::module::ServerModuleInit;
    use crate::net::api::ApiSecrets;

    /// Helper in config API tests for simulating a guardian's client and server
    struct TestConfigApi {
        client: DynGlobalApi,
        auth: ApiAuth,
        name: String,
        settings: ConfigGenSettings,
        amount: Amount,
        dir: PathBuf,
    }

    impl TestConfigApi {
        /// Creates a new test API taking up a port, with P2P endpoint on the
        /// next port
        fn new(port: u16, name_suffix: u16, data_dir: &Path) -> TestConfigApi {
            let db = MemDatabase::new().into_database();

            let name = format!("peer{name_suffix}");
            let api_bind = format!("127.0.0.1:{port}").parse().expect("parses");
            let api_url: SafeUrl = format!("ws://127.0.0.1:{port}").parse().expect("parses");
            let p2p_bind = format!("127.0.0.1:{}", port + 1).parse().expect("parses");
            let p2p_url = format!("fedimint://127.0.0.1:{}", port + 1)
                .parse()
                .expect("parses");
            let module_inits = ServerModuleInitRegistry::from_iter([DummyInit.into()]);
            let mut modules = ServerModuleConfigGenParamsRegistry::default();
            modules.attach_config_gen_params_by_id(0, DummyInit::kind(), DummyGenParams::default());

            let default_params = ConfigGenParamsRequest {
                meta: Default::default(),
                modules,
            };
            let settings = ConfigGenSettings {
                download_token_limit: None,
                p2p_bind,
                api_bind,
                p2p_url,
                api_url: api_url.clone(),
                default_params,
                max_connections: DEFAULT_MAX_CLIENT_CONNECTIONS,
                registry: ServerModuleInitRegistry::from(vec![DynServerModuleInit::from(
                    DummyInit,
                )]),
            };

            let dir = data_dir.join(name_suffix.to_string());
            fs::create_dir_all(dir.clone()).expect("Unable to create test dir");

            let dir_clone = dir.clone();
            let settings_clone = settings.clone();

            spawn("fedimint server", async move {
                crate::run(
                    dir_clone,
                    ApiSecrets::none(),
                    settings_clone,
                    db,
                    "dummyversionhash".to_owned(),
                    &module_inits,
                    TaskGroup::new(),
                )
                .await
                .expect("Failed to run fedimint server");
            });

            // our id doesn't really exist at this point
            let auth = ApiAuth(format!("password-{port}"));
            let client = DynGlobalApi::from_pre_peer_id_admin_endpoint(api_url, &None);

            TestConfigApi {
                client,
                auth,
                name,
                settings,
                amount: Amount::from_sats(u64::from(port)),
                dir,
            }
        }

        /// Helper function using generated urls
        async fn set_connections(&self, leader: &Option<SafeUrl>) -> FederationResult<()> {
            self.client
                .set_config_gen_connections(
                    ConfigGenConnectionsRequest {
                        our_name: self.name.clone(),
                        leader_api_url: leader.clone(),
                    },
                    self.auth.clone(),
                )
                .await
        }

        /// Helper for getting server status
        async fn status(&self) -> StatusResponse {
            loop {
                match self.client.status().await {
                    Ok(status) => return status,
                    Err(_) => sleep(Duration::from_millis(1000)).await,
                }
                info!(
                    target: fedimint_logging::LOG_TEST,
                    "Test retrying server status"
                );
            }
        }

        /// Helper for awaiting all servers have the status
        /// Use this BEFORE server config gen params have been set
        async fn wait_status_preconfig(&self, status: ServerStatus, peers: &Vec<TestConfigApi>) {
            loop {
                let server_status = self.status().await.server;
                if server_status == status {
                    for peer in peers {
                        let peer_status = peer.status().await.server;
                        if peer_status != server_status {
                            info!(
                                target: fedimint_logging::LOG_TEST,
                                "Test retrying peer server status preconfig"
                            );
                            sleep(Duration::from_millis(10)).await;
                            continue;
                        }
                    }
                    break;
                }
                info!(
                    target: fedimint_logging::LOG_TEST,
                    "Test retrying server status preconfig"
                );
            }
        }

        /// Helper for awaiting all servers have the status
        /// Use this AFTER server config gen params have been set
        async fn wait_status(&self, status: ServerStatus) {
            loop {
                let response = self.client.consensus_config_gen_params().await.unwrap();
                let mismatched: Vec<_> = response
                    .consensus
                    .peers
                    .iter()
                    .filter(|(_, param)| param.status != Some(status.clone()))
                    .collect();
                if mismatched.is_empty() {
                    break;
                }
                info!(
                    target: fedimint_logging::LOG_TEST,
                    "Test retrying server status"
                );
                sleep(Duration::from_millis(10)).await;
            }
        }

        /// Sets local param to name and unique consensus amount for testing
        async fn set_config_gen_params(&self) {
            let mut modules = ServerModuleConfigGenParamsRegistry::default();
            modules.attach_config_gen_params_by_id(
                0,
                DummyInit::kind(),
                DummyGenParams {
                    local: DummyGenParamsLocal,
                    consensus: DummyGenParamsConsensus {
                        tx_fee: self.amount,
                    },
                },
            );
            let request = ConfigGenParamsRequest {
                meta: BTreeMap::from([("\"test\"".to_string(), self.name.clone())]),
                modules,
            };

            self.client
                .set_config_gen_params(request, self.auth.clone())
                .await
                .unwrap();
        }

        /// reads the dummy module config from the filesystem
        fn read_config(&self) -> ServerConfig {
            let auth = fs::read_to_string(self.dir.join(PLAINTEXT_PASSWORD));
            read_server_config(&auth.unwrap(), &self.dir).unwrap()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_config_api() {
        const PEER_NUM: u16 = 4;
        const PORTS_PER_PEER: u16 = 2;
        let _ = TracingSetup::default().init();
        let (data_dir, _maybe_tmp_dir_guard) = test_dir("test-config-api");
        let base_port = port_alloc(PEER_NUM * PORTS_PER_PEER).unwrap();

        let mut followers = vec![];
        let mut test_config = TestConfigApi::new(base_port, 0, &data_dir);

        for i in 1..PEER_NUM {
            let port = base_port + (i * PORTS_PER_PEER);
            let follower = TestConfigApi::new(port, i, &data_dir);
            followers.push(follower);
        }

        test_config = validate_leader_setup(test_config).await;

        // Setup followers and send connection info
        for follower in &mut followers {
            assert_eq!(
                follower.status().await.server,
                ServerStatus::AwaitingPassword
            );
            follower
                .client
                .set_password(follower.auth.clone())
                .await
                .unwrap();
            let leader_url = Some(test_config.settings.api_url.clone());
            follower.set_connections(&leader_url).await.unwrap();
            follower.name = format!("{}_", follower.name);
            follower.set_connections(&leader_url).await.unwrap();
            follower.set_config_gen_params().await;
        }

        // Validate we can do a full fedimint setup
        validate_full_setup(test_config, followers).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore] // TODO: flaky https://github.com/fedimint/fedimint/issues/4308
    async fn test_restart_setup() {
        const PEER_NUM: u16 = 4;
        const PORTS_PER_PEER: u16 = 2;
        let _ = TracingSetup::default().init();
        let (data_dir, _maybe_tmp_dir_guard) = test_dir("test-restart-setup");
        let base_port = port_alloc(PEER_NUM * PORTS_PER_PEER).unwrap();

        let mut followers = vec![];
        let mut test_config = TestConfigApi::new(base_port, 0, &data_dir);

        for i in 1..PEER_NUM {
            let port = base_port + (i * PORTS_PER_PEER);
            let follower = TestConfigApi::new(port, i, &data_dir);
            followers.push(follower);
        }

        test_config = validate_leader_setup(test_config).await;

        // Setup followers and send connection info
        for follower in &mut followers {
            assert_eq!(
                follower.status().await.server,
                ServerStatus::AwaitingPassword
            );
            follower
                .client
                .set_password(follower.auth.clone())
                .await
                .unwrap();
            let leader_url = Some(test_config.settings.api_url.clone());
            follower.set_connections(&leader_url).await.unwrap();
            follower.name = format!("{}_", follower.name);
            follower.set_connections(&leader_url).await.unwrap();
            follower.set_config_gen_params().await;
        }
        test_config
            .wait_status(ServerStatus::SharingConfigGenParams)
            .await;

        // Leader can trigger a setup restart
        test_config
            .client
            .restart_federation_setup(test_config.auth.clone())
            .await
            .unwrap();

        // All peers can trigger a setup restart. This has to be done manually by each
        // peer, and any peer could trigger a restart before the leader does.
        for peer in &followers {
            peer.client
                .restart_federation_setup(peer.auth.clone())
                .await
                .ok();
        }

        // Ensure all servers have restarted
        test_config
            .wait_status_preconfig(ServerStatus::SetupRestarted, &followers)
            .await;
        test_config
            .wait_status_preconfig(ServerStatus::AwaitingPassword, &followers)
            .await;

        test_config = validate_leader_setup(test_config).await;

        // Setup followers and send connection info
        for follower in &mut followers {
            assert_eq!(
                follower.status().await.server,
                ServerStatus::AwaitingPassword
            );
            follower
                .client
                .set_password(follower.auth.clone())
                .await
                .unwrap();
            let leader_url = Some(test_config.settings.api_url.clone());
            follower.set_connections(&leader_url).await.unwrap();
            follower.set_config_gen_params().await;
        }

        // Validate we can do a full fedimint setup after a restart
        validate_full_setup(test_config, followers).await;
    }

    // Validate steps when leader initiates fedimint setup
    async fn validate_leader_setup(mut leader: TestConfigApi) -> TestConfigApi {
        assert_eq!(leader.status().await.server, ServerStatus::AwaitingPassword);

        // Cannot set the password twice
        leader
            .client
            .set_password(leader.auth.clone())
            .await
            .unwrap();
        assert!(leader
            .client
            .set_password(leader.auth.clone())
            .await
            .is_err());

        // We can call this twice to change the leader name
        leader.set_connections(&None).await.unwrap();
        leader.name = "leader".to_string();
        leader.set_connections(&None).await.unwrap();

        // Leader sets the config
        let _ = leader
            .client
            .get_default_config_gen_params(leader.auth.clone())
            .await
            .unwrap();
        leader.set_config_gen_params().await;

        leader
    }

    // Validate we can use the config api to do a full fedimint setup
    async fn validate_full_setup(leader: TestConfigApi, mut followers: Vec<TestConfigApi>) {
        // Confirm we can get peer servers if we are the leader
        let peers = leader.client.get_config_gen_peers().await.unwrap();
        let names: Vec<_> = peers.into_iter().map(|peer| peer.name).sorted().collect();
        assert_eq!(names, vec!["leader", "peer1_", "peer2_", "peer3_"]);

        leader
            .wait_status(ServerStatus::SharingConfigGenParams)
            .await;

        // Followers can fetch configs
        let mut configs = vec![];
        for peer in &followers {
            configs.push(peer.client.consensus_config_gen_params().await.unwrap());
        }
        // Confirm all consensus configs are the same
        let mut consensus: Vec<_> = configs.iter().map(|p| p.consensus.clone()).collect();
        consensus.dedup();
        assert_eq!(consensus.len(), 1);
        // Confirm all peer ids are unique
        let ids: BTreeSet<_> = configs.iter().map(|p| p.our_current_id).collect();
        assert_eq!(ids.len(), followers.len());

        // all peers run DKG
        let leader_amount = leader.amount;
        let leader_name = leader.name.clone();
        followers.push(leader);
        let all_peers = Arc::new(followers);
        let (results, _) = tokio::join!(
            join_all(
                all_peers
                    .iter()
                    .map(|peer| peer.client.run_dkg(peer.auth.clone()))
            ),
            all_peers[0].wait_status(ServerStatus::VerifyingConfigs)
        );
        for result in results {
            result.expect("DKG failed");
        }

        // verify config hashes equal for all peers
        let mut hashes = HashSet::new();
        for peer in all_peers.iter() {
            peer.wait_status(ServerStatus::VerifyingConfigs).await;
            hashes.insert(
                peer.client
                    .get_verify_config_hash(peer.auth.clone())
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(hashes.len(), 1);

        // set verified configs
        for peer in all_peers.iter() {
            peer.client.verified_configs(peer.auth.clone()).await.ok();
        }

        // start consensus
        for peer in all_peers.iter() {
            peer.client.start_consensus(peer.auth.clone()).await.ok();
        }

        sleep(Duration::from_secs(5)).await;

        for peer in all_peers.iter() {
            assert_eq!(peer.status().await.server, ServerStatus::ConsensusRunning);

            // verify the local and consensus values for peers
            let cfg = peer.read_config(); // read persisted configs
            let dummy: DummyConfig = cfg.get_module_config_typed(0).unwrap();
            assert_eq!(dummy.consensus.tx_fee, leader_amount);
            assert_eq!(cfg.consensus.meta["\"test\""], leader_name);
        }
    }
}
