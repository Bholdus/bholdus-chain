//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

pub use phoenix_runtime::RuntimeApi;

use bholdus_primitives::Block;
use frame_system_rpc_runtime_api::AccountNonceApi;
use futures::prelude::*;
use sc_client_api::{BlockBackend, ExecutorProvider, RemoteBackend};
use sc_consensus_aura::{self, ImportQueueParams, SlotProportion, StartAuraParams};
pub use sc_executor::NativeElseWasmExecutor;
use sc_finality_grandpa::{self as grandpa};
use sc_keystore::LocalKeystore;
use sc_network::{Event, NetworkService};
use sc_service::{config::Configuration, error::Error as ServiceError, RpcHandlers, TaskManager};
use sc_telemetry::{Telemetry, TelemetryWorker};
use sp_api::ProvideRuntimeApi;
use sp_consensus::SlotData;
use sp_consensus_aura::sr25519::AuthorityPair as AuraPair;
use sp_core::{Encode, Pair};
use sp_runtime::{generic, traits::Block as BlockT, SaturatedConversion};
use std::sync::Arc;

use bholdus_rpc;

// Our native executor instance.
pub struct ExecutorDispatch;

// #[cfg(feature = "with-bholdus-runtime")]
impl sc_executor::NativeExecutionDispatch for ExecutorDispatch {
    /// Only enable the benchmarking host functions when we actually want to benchmark.
    #[cfg(feature = "runtime-benchmarks")]
    type ExtendHostFunctions = frame_benchmarking::benchmarking::HostFunctions;
    /// Otherwise we only use the default Substrate host functions.
    #[cfg(not(feature = "runtime-benchmarks"))]
    type ExtendHostFunctions = ();

    fn dispatch(method: &str, data: &[u8]) -> Option<Vec<u8>> {
        phoenix_runtime::api::dispatch(method, data)
    }

    fn native_version() -> sc_executor::NativeVersion {
        phoenix_runtime::native_version()
    }
}

type FullClient =
    sc_service::TFullClient<Block, RuntimeApi, NativeElseWasmExecutor<ExecutorDispatch>>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;
type FullGrandpaBlockImport =
    sc_finality_grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>;
/// The transaction pool type defintion.
pub type TransactionPool = sc_transaction_pool::FullPool<Block, FullClient>;

/// Fetch the nonce of the given `account` from the chain state.
///
/// Note: Should only be used for tests.
pub fn fetch_nonce(client: &FullClient, account: sp_core::sr25519::Pair) -> u32 {
    let best_hash = client.chain_info().best_hash;
    client
        .runtime_api()
        .account_nonce(&generic::BlockId::Hash(best_hash), account.public().into())
        .expect("Fetching account nonce works; qed")
}

/// Create a transaction using the given `call`.
///
/// The transaction will be signed by `sender`. If `nonce` is `None` it will be fetched from the
/// state of the best block.
///
/// Note: Should only be used for tests.
pub fn create_extrinsic(
    client: &FullClient,
    sender: sp_core::sr25519::Pair,
    function: impl Into<phoenix_runtime::Call>,
    nonce: Option<u32>,
) -> phoenix_runtime::UncheckedExtrinsic {
    let function = function.into();
    let genesis_hash = client
        .block_hash(0)
        .ok()
        .flatten()
        .expect("Genesis block exists; qed");
    let best_hash = client.chain_info().best_hash;
    let best_block = client.chain_info().best_number;
    let nonce = nonce.unwrap_or_else(|| fetch_nonce(client, sender.clone()));

    let period = phoenix_runtime::BlockHashCount::get()
        .checked_next_power_of_two()
        .map(|c| c / 2)
        .unwrap_or(2) as u64;
    let tip = 0;
    let extra: phoenix_runtime::SignedExtra = (
        frame_system::CheckSpecVersion::<phoenix_runtime::Runtime>::new(),
        frame_system::CheckTxVersion::<phoenix_runtime::Runtime>::new(),
        frame_system::CheckGenesis::<phoenix_runtime::Runtime>::new(),
        frame_system::CheckEra::<phoenix_runtime::Runtime>::from(generic::Era::mortal(
            period,
            best_block.saturated_into(),
        )),
        frame_system::CheckNonce::<phoenix_runtime::Runtime>::from(nonce),
        frame_system::CheckWeight::<phoenix_runtime::Runtime>::new(),
        pallet_transaction_payment::ChargeTransactionPayment::<phoenix_runtime::Runtime>::from(tip),
    );

    let raw_payload = phoenix_runtime::SignedPayload::from_raw(
        function.clone(),
        extra.clone(),
        (
            phoenix_runtime::VERSION.spec_version,
            phoenix_runtime::VERSION.transaction_version,
            genesis_hash,
            best_hash,
            (),
            (),
            (),
        ),
    );
    let signature = raw_payload.using_encoded(|e| sender.sign(e));

    phoenix_runtime::UncheckedExtrinsic::new_signed(
        function.clone(),
        sp_runtime::AccountId32::from(sender.public()).into(),
        phoenix_runtime::Signature::Sr25519(signature.clone()),
        extra.clone(),
    )
}

pub fn new_partial(
    config: &Configuration,
) -> Result<
    sc_service::PartialComponents<
        FullClient,
        FullBackend,
        FullSelectChain,
        sc_consensus::DefaultImportQueue<Block, FullClient>,
        sc_transaction_pool::FullPool<Block, FullClient>,
        (
            impl sc_service::RpcExtensionBuilder,
            (sc_finality_grandpa::SharedVoterState,),
            (
                sc_finality_grandpa::GrandpaBlockImport<
                    FullBackend,
                    Block,
                    FullClient,
                    FullSelectChain,
                >,
                sc_finality_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
                beefy_gadget::notification::BeefySignedCommitmentSender<Block>,
            ),
            Option<Telemetry>,
        ),
    >,
    ServiceError,
> {
    let telemetry = config
        .telemetry_endpoints
        .clone()
        .filter(|x| !x.is_empty())
        .map(|endpoints| -> Result<_, sc_telemetry::Error> {
            let worker = TelemetryWorker::new(16)?;
            let telemetry = worker.handle().new_telemetry(endpoints);
            Ok((worker, telemetry))
        })
        .transpose()?;

    let executor = NativeElseWasmExecutor::<ExecutorDispatch>::new(
        config.wasm_method,
        config.default_heap_pages,
        config.max_runtime_instances,
    );

    let (client, backend, keystore_container, task_manager) =
        sc_service::new_full_parts::<Block, RuntimeApi, _>(
            &config,
            telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
            executor,
        )?;
    let client = Arc::new(client);

    let telemetry = telemetry.map(|(worker, telemetry)| {
        task_manager.spawn_handle().spawn("telemetry", worker.run());
        telemetry
    });

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

    let transaction_pool = sc_transaction_pool::BasicPool::new_full(
        config.transaction_pool.clone(),
        config.role.is_authority().into(),
        config.prometheus_registry(),
        task_manager.spawn_essential_handle(),
        client.clone(),
    );

    let (grandpa_block_import, grandpa_link) = grandpa::block_import(
        client.clone(),
        &(client.clone() as Arc<_>),
        select_chain.clone(),
        telemetry.as_ref().map(|x| x.handle()),
    )?;

    let slot_duration = sc_consensus_aura::slot_duration(&*client)?.slot_duration();

    let import_queue =
        sc_consensus_aura::import_queue::<AuraPair, _, _, _, _, _, _>(ImportQueueParams {
            block_import: grandpa_block_import.clone(),
            justification_import: Some(Box::new(grandpa_block_import.clone())),
            client: client.clone(),
            create_inherent_data_providers: move |_, ()| async move {
                let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                let slot =
                    sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
                        *timestamp,
                        slot_duration,
                    );

                Ok((timestamp, slot))
            },
            spawner: &task_manager.spawn_essential_handle(),
            can_author_with: sp_consensus::CanAuthorWithNativeVersion::new(
                client.executor().clone(),
            ),
            registry: config.prometheus_registry(),
            check_for_equivocation: Default::default(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
        })?;

    let (beefy_link, beefy_commitment_stream) =
        beefy_gadget::notification::BeefySignedCommitmentStream::channel();

    let import_setup = (grandpa_block_import.clone(), grandpa_link, beefy_link);

    let (rpc_extensions_builder, rpc_setup) = {
        let (_, grandpa_link, _) = &import_setup;
        let justification_stream = grandpa_link.justification_stream();
        let shared_authority_set = grandpa_link.shared_authority_set().clone();
        let shared_voter_state = sc_finality_grandpa::SharedVoterState::empty();
        let finality_proof_provider = sc_finality_grandpa::FinalityProofProvider::new_for_service(
            backend.clone(),
            Some(shared_authority_set.clone()),
        );

        let rpc_setup = (shared_voter_state.clone(),);
        let client = client.clone();
        let pool = transaction_pool.clone();
        let select_chain = select_chain.clone();
        let is_authority = config.role.clone().is_authority();
        let _keystore = keystore_container.sync_keystore();
        let chain_spec = config.chain_spec.cloned_box();

        let rpc_extensions_builder =
            move |deny_unsafe, subscription_executor: bholdus_rpc::SubscriptionTaskExecutor| {
                let deps = bholdus_rpc::FullDeps {
                    client: client.clone(),
                    pool: pool.clone(),
                    select_chain: select_chain.clone(),
                    deny_unsafe,
                    chain_spec: chain_spec.cloned_box(),
                    // Grandpa
                    grandpa: bholdus_rpc::GrandpaDeps {
                        shared_voter_state: shared_voter_state.clone(),
                        shared_authority_set: shared_authority_set.clone(),
                        justification_stream: justification_stream.clone(),
                        subscription_executor: subscription_executor.clone(),
                        finality_provider: finality_proof_provider.clone(),
                    },
                    beefy: bholdus_rpc::BeefyDeps {
                        beefy_commitment_stream: beefy_commitment_stream.clone(),
                        subscription_executor,
                    },
                };

                bholdus_rpc::create_full(deps).map_err(Into::into)
            };

        (rpc_extensions_builder, rpc_setup)
    };

    Ok(sc_service::PartialComponents {
        client,
        backend,
        task_manager,
        import_queue,
        keystore_container,
        select_chain,
        transaction_pool,
        other: (rpc_extensions_builder, rpc_setup, import_setup, telemetry),
    })
}

pub struct NewFullBase {
    pub task_manager: TaskManager,
    pub client: Arc<FullClient>,
    pub network: Arc<NetworkService<Block, <Block as BlockT>::Hash>>,
    pub transaction_pool: Arc<sc_transaction_pool::FullPool<Block, FullClient>>,
}

fn remote_keystore(_url: &String) -> Result<Arc<LocalKeystore>, &'static str> {
    // FIXME: here would the concrete keystore be built,
    //        must return a concrete type (NOT `LocalKeystore`) that
    //        implements `CryptoStore` and `SyncCryptoStore`
    Err("Remote Keystore not supported.")
}

/// Creates a full service from the configuration.
pub fn new_full_base(mut config: Configuration) -> Result<NewFullBase, ServiceError> {
    let sc_service::PartialComponents {
        client,
        backend,
        mut task_manager,
        import_queue,
        mut keystore_container,
        select_chain,
        transaction_pool,
        other: (rpc_extensions_builder, rpc_setup, import_setup, mut telemetry),
    } = new_partial(&config)?;

    if let Some(url) = &config.keystore_remote {
        match remote_keystore(url) {
            Ok(k) => keystore_container.set_remote_keystore(k),
            Err(e) => {
                return Err(ServiceError::Other(format!(
                    "Error hooking up remote keystore for {}: {}",
                    url, e
                )))
            }
        };
    }

    config
        .network
        .extra_sets
        .push(grandpa::grandpa_peers_set_config());

    config
        .network
        .extra_sets
        .push(beefy_gadget::beefy_peers_set_config());

    let (grandpa_block_import, grandpa_link, beefy_signed_commitment_sender) = import_setup;

    let warp_sync = Arc::new(grandpa::warp_proof::NetworkProvider::new(
        backend.clone(),
        grandpa_link.shared_authority_set().clone(),
        Vec::default(),
    ));

    let (network, system_rpc_tx, network_starter) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            on_demand: None,
            block_announce_validator_builder: None,
            warp_sync: Some(warp_sync),
        })?;

    if config.offchain_worker.enabled {
        sc_service::build_offchain_workers(
            &config,
            task_manager.spawn_handle(),
            client.clone(),
            network.clone(),
        );
    }

    let role = config.role.clone();
    let force_authoring = config.force_authoring;
    let backoff_authoring_blocks: Option<()> = None;
    let name = config.network.node_name.clone();
    let enable_grandpa = !config.disable_grandpa;
    let prometheus_registry = config.prometheus_registry().cloned();

    let (shared_voter_state,) = rpc_setup;

    let _rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
        config,
        backend: backend.clone(),
        client: client.clone(),
        keystore: keystore_container.sync_keystore(),
        network: network.clone(),
        rpc_extensions_builder: Box::new(rpc_extensions_builder),
        transaction_pool: transaction_pool.clone(),
        task_manager: &mut task_manager,
        on_demand: None,
        remote_blockchain: None,
        system_rpc_tx,
        telemetry: telemetry.as_mut(),
    })?;

    if let sc_service::config::Role::Authority { .. } = &role {
        let proposer_factory = sc_basic_authorship::ProposerFactory::new(
            task_manager.spawn_handle(),
            client.clone(),
            transaction_pool.clone(),
            prometheus_registry.as_ref(),
            telemetry.as_ref().map(|x| x.handle()),
        );

        let can_author_with =
            sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone());
        let slot_duration = sc_consensus_aura::slot_duration(&*client)?;
        let raw_duration = slot_duration.slot_duration();

        let aura = sc_consensus_aura::start_aura::<AuraPair, _, _, _, _, _, _, _, _, _, _, _>(
            StartAuraParams {
                slot_duration,
                client: client.clone(),
                select_chain,
                block_import: grandpa_block_import.clone(),
                proposer_factory,
                create_inherent_data_providers: move |_, ()| async move {
                    let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                    let slot =
                    sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
                        *timestamp,
                        raw_duration,
                    );

                    Ok((timestamp, slot))
                },
                force_authoring,
                backoff_authoring_blocks,
                keystore: keystore_container.sync_keystore(),
                can_author_with,
                sync_oracle: network.clone(),
                justification_sync_link: network.clone(),
                block_proposal_slot_portion: SlotProportion::new(2f32 / 3f32),
                max_block_proposal_slot_portion: None,
                telemetry: telemetry.as_ref().map(|x| x.handle()),
            },
        )?;

        task_manager
            .spawn_essential_handle()
            .spawn_blocking("aura-proposer", aura);
    }

    // Spawn authority discovery module.
    if role.is_authority() {
        let authority_discovery_role =
            sc_authority_discovery::Role::PublishAndDiscover(keystore_container.keystore());
        let dht_event_stream =
            network
                .event_stream("authority-discovery")
                .filter_map(|e| async move {
                    match e {
                        Event::Dht(e) => Some(e),
                        _ => None,
                    }
                });
        let (authority_discovery_worker, _service) = sc_authority_discovery::new_worker_and_service(
            client.clone(),
            network.clone(),
            Box::pin(dht_event_stream),
            authority_discovery_role,
            prometheus_registry.clone(),
        );

        task_manager.spawn_handle().spawn(
            "authority-discovery-worker",
            authority_discovery_worker.run(),
        );
    }

    // if the node isn't actively participating in consensus then it doesn't
    // need a keystore, regardless of which protocol we use below.
    let keystore = if role.is_authority() {
        Some(keystore_container.sync_keystore())
    } else {
        None
    };

    let beefy_params = beefy_gadget::BeefyParams {
        client: client.clone(),
        backend,
        key_store: keystore.clone(),
        network: network.clone(),
        signed_commitment_sender: beefy_signed_commitment_sender,
        min_block_delta: 4,
        prometheus_registry: prometheus_registry.clone(),
    };

    // Start BEEFY bridge gadget
    task_manager.spawn_essential_handle().spawn_blocking(
        "beefy-gadget",
        beefy_gadget::start_beefy_gadget::<_, _, _, _>(beefy_params),
    );

    let config = grandpa::Config {
        // FIXME #1578 make this available through chainspec
        gossip_duration: std::time::Duration::from_millis(333),
        justification_period: 512,
        name: Some(name),
        observer_enabled: false,
        keystore,
        local_role: role,
        telemetry: telemetry.as_ref().map(|x| x.handle()),
    };

    if enable_grandpa {
        // start the full GRANDPA voter
        // NOTE: non-authorities could run the GRANDPA observer protocol, but at
        // this point the full voter should provide better guarantees of block
        // and vote data availability than the observer. The observer has not
        // been tested extensively yet and having most nodes in a network run it
        // could lead to finality stalls.
        let grandpa_config = grandpa::GrandpaParams {
            config,
            link: grandpa_link,
            network: network.clone(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
            voting_rule: grandpa::VotingRulesBuilder::default().build(),
            prometheus_registry,
            shared_voter_state,
        };

        // the GRANDPA voter task is considered infallible, i.e.
        // if it fails we take down the service with it.
        task_manager
            .spawn_essential_handle()
            .spawn_blocking("grandpa-voter", grandpa::run_grandpa_voter(grandpa_config)?);
    }

    network_starter.start_network();
    Ok(NewFullBase {
        task_manager,
        client: client.clone(),
        network,
        transaction_pool,
    })
}

/// Builds a new service for a full client.
pub fn new_full(config: Configuration) -> Result<TaskManager, ServiceError> {
    new_full_base(config).map(|NewFullBase { task_manager, .. }| task_manager)
}
