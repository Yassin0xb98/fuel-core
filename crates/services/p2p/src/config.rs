use crate::{
    gossipsub::config::default_gossipsub_config,
    heartbeat::HeartbeatConfig,
    peer_manager::ConnectionState,
};
use fuel_core_types::blockchain::consensus::Genesis;

use libp2p::{
    core::{
        muxing::StreamMuxerBox,
        transport::Boxed,
    },
    gossipsub::GossipsubConfig,
    identity::{
        secp256k1::SecretKey,
        Keypair,
    },
    mplex,
    noise::{self,},
    tcp::{
        tokio::Transport as TokioTcpTransport,
        Config as TcpConfig,
    },
    yamux,
    Multiaddr,
    PeerId,
    Transport,
};
use std::{
    collections::HashSet,
    net::{
        IpAddr,
        Ipv4Addr,
    },
    sync::{
        Arc,
        RwLock,
    },
    time::Duration,
};

use self::{
    connection_tracker::ConnectionTracker,
    fuel_authenticated::FuelAuthenticated,
    fuel_upgrade::{
        Checksum,
        FuelUpgrade,
    },
    guarded_node::GuardedNode,
};
mod connection_tracker;
mod fuel_authenticated;
mod fuel_upgrade;
mod guarded_node;

const REQ_RES_TIMEOUT: Duration = Duration::from_secs(20);

/// Maximum response size from the p2p.
/// The configuration of the ingress should be the same:
/// - `nginx.org/client-max-body-size`
/// - `nginx.ingress.kubernetes.io/proxy-body-size`
pub const MAX_RESPONSE_SIZE: usize = 18 * 1024 * 1024;

/// Maximum number of headers per request.
pub const MAX_HEADERS_PER_REQUEST: u32 = 100;

/// Adds a timeout to the setup and protocol upgrade process for all
/// inbound and outbound connections established through the transport.
const TRANSPORT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct Config<State = Initialized> {
    /// The keypair used for for handshake during communication with other p2p nodes.
    pub keypair: Keypair,

    /// Name of the Network
    pub network_name: String,

    /// Checksum is a hash(sha256) of [`Genesis`](fuel_core_types::blockchain::consensus::Genesis) - chain id.
    pub checksum: Checksum,

    /// IP address for Swarm to listen on
    pub address: IpAddr,

    /// Optional address of your local node made reachable for other nodes in the network.
    pub public_address: Option<Multiaddr>,

    /// The TCP port that Swarm listens on
    pub tcp_port: u16,

    /// Max Size of a Block in bytes
    pub max_block_size: usize,
    pub max_headers_per_request: u32,

    // `DiscoveryBehaviour` related fields
    pub bootstrap_nodes: Vec<Multiaddr>,
    pub enable_mdns: bool,
    pub allow_private_addresses: bool,
    pub random_walk: Option<Duration>,
    pub connection_idle_timeout: Option<Duration>,

    // 'Reserved Nodes' mode
    /// Priority nodes that the node should maintain connection to
    pub reserved_nodes: Vec<Multiaddr>,
    /// Should the node only accept connection requests from the Reserved Nodes
    pub reserved_nodes_only_mode: bool,

    // `PeerManager` fields
    /// Max number of unique peers connected
    /// This number should be at least number of `mesh_n` from `Gossipsub` configuration.
    /// The total number of connections will be `(max_peers_connected + reserved_nodes.len()) * max_connections_per_peer`
    pub max_peers_connected: u32,
    /// Max number of connections per single peer
    /// The total number of connections will be `(max_peers_connected + reserved_nodes.len()) * max_connections_per_peer`
    pub max_connections_per_peer: u32,
    /// The interval at which identification requests are sent to
    /// the remote on established connections after the first request
    pub identify_interval: Option<Duration>,
    /// The duration between the last successful outbound or inbound ping
    /// and the next outbound ping
    pub info_interval: Option<Duration>,

    // `Gossipsub` config
    pub gossipsub_config: GossipsubConfig,

    pub heartbeat_config: HeartbeatConfig,

    // RequestResponse related fields
    /// Sets the timeout for inbound and outbound requests.
    pub set_request_timeout: Duration,
    /// Sets the keep-alive timeout of idle connections.
    pub set_connection_keep_alive: Duration,

    /// Time between checking heartbeat status for all peers
    pub heartbeat_check_interval: Duration,
    /// Max avg time between heartbeats for a given peer before getting reputation penalty
    pub heartbeat_max_avg_interval: Duration,
    /// Max time since a given peer has sent a heartbeat before getting reputation penalty
    pub heartbeat_max_time_since_last: Duration,

    /// Enables prometheus metrics for this fuel-service
    pub metrics: bool,

    /// It is the state of the config initialization. Everyone can create an instance of the `Self`
    /// with the `NotInitialized` state. But it can be set into the `Initialized` state only with
    /// the `init` method.
    pub state: State,
}

/// The initialized state can be achieved only by the `init` function because `()` is private.
#[derive(Clone, Debug)]
pub struct Initialized(());

#[derive(Clone, Debug)]
pub struct NotInitialized;

impl Config<NotInitialized> {
    /// Inits the `P2PConfig` with some lazily loaded data.
    pub fn init(self, genesis: Genesis) -> anyhow::Result<Config<Initialized>> {
        use fuel_core_chain_config::GenesisCommitment;

        Ok(Config {
            keypair: self.keypair,
            network_name: self.network_name,
            checksum: genesis.root()?.into(),
            address: self.address,
            public_address: self.public_address,
            tcp_port: self.tcp_port,
            max_block_size: self.max_block_size,
            max_headers_per_request: self.max_headers_per_request,
            bootstrap_nodes: self.bootstrap_nodes,
            enable_mdns: self.enable_mdns,
            max_peers_connected: self.max_peers_connected,
            max_connections_per_peer: self.max_connections_per_peer,
            allow_private_addresses: self.allow_private_addresses,
            random_walk: self.random_walk,
            connection_idle_timeout: self.connection_idle_timeout,
            reserved_nodes: self.reserved_nodes,
            reserved_nodes_only_mode: self.reserved_nodes_only_mode,
            identify_interval: self.identify_interval,
            info_interval: self.info_interval,
            gossipsub_config: self.gossipsub_config,
            heartbeat_config: self.heartbeat_config,
            set_request_timeout: self.set_request_timeout,
            set_connection_keep_alive: self.set_connection_keep_alive,
            heartbeat_check_interval: self.heartbeat_check_interval,
            heartbeat_max_avg_interval: self.heartbeat_max_time_since_last,
            heartbeat_max_time_since_last: self.heartbeat_max_time_since_last,
            metrics: self.metrics,
            state: Initialized(()),
        })
    }
}

/// Takes secret key bytes generated outside of libp2p.
/// And converts it into libp2p's `Keypair::Secp256k1`.
pub fn convert_to_libp2p_keypair(
    secret_key_bytes: impl AsMut<[u8]>,
) -> anyhow::Result<Keypair> {
    let secret_key = SecretKey::from_bytes(secret_key_bytes)?;

    Ok(Keypair::Secp256k1(secret_key.into()))
}

impl Config<NotInitialized> {
    pub fn default(network_name: &str) -> Self {
        let keypair = Keypair::generate_secp256k1();

        Self {
            keypair,
            network_name: network_name.into(),
            checksum: Default::default(),
            address: IpAddr::V4(Ipv4Addr::from([0, 0, 0, 0])),
            public_address: None,
            tcp_port: 0,
            max_block_size: MAX_RESPONSE_SIZE,
            max_headers_per_request: MAX_HEADERS_PER_REQUEST,
            bootstrap_nodes: vec![],
            enable_mdns: false,
            max_peers_connected: 50,
            max_connections_per_peer: 3,
            allow_private_addresses: true,
            random_walk: Some(Duration::from_millis(500)),
            connection_idle_timeout: Some(Duration::from_secs(120)),
            reserved_nodes: vec![],
            reserved_nodes_only_mode: false,
            gossipsub_config: default_gossipsub_config(),
            heartbeat_config: HeartbeatConfig::default(),
            set_request_timeout: REQ_RES_TIMEOUT,
            set_connection_keep_alive: REQ_RES_TIMEOUT,
            heartbeat_check_interval: Duration::from_secs(10),
            heartbeat_max_avg_interval: Duration::from_secs(20),
            heartbeat_max_time_since_last: Duration::from_secs(40),
            info_interval: Some(Duration::from_secs(3)),
            identify_interval: Some(Duration::from_secs(5)),
            metrics: false,
            state: NotInitialized,
        }
    }
}

#[cfg(any(feature = "test-helpers", test))]
impl Config<Initialized> {
    pub fn default_initialized(network_name: &str) -> Self {
        Config::<NotInitialized>::default(network_name)
            .init(Default::default())
            .expect("Expected correct initialization of config")
    }
}

/// Transport for libp2p communication:
/// TCP/IP, Websocket
/// Noise as encryption layer
/// mplex or yamux for multiplexing
pub(crate) fn build_transport(
    p2p_config: &Config,
) -> (
    Boxed<(PeerId, StreamMuxerBox)>,
    Arc<RwLock<ConnectionState>>,
) {
    let transport = {
        let generate_tcp_transport =
            || TokioTcpTransport::new(TcpConfig::new().port_reuse(true).nodelay(true));

        let tcp = generate_tcp_transport();

        let ws_tcp =
            libp2p::websocket::WsConfig::new(generate_tcp_transport()).or_transport(tcp);

        libp2p::dns::TokioDnsConfig::system(ws_tcp).unwrap()
    }
    .upgrade(libp2p::core::upgrade::Version::V1);

    let noise_authenticated = {
        let dh_keys = noise::Keypair::<noise::X25519Spec>::new()
            .into_authentic(&p2p_config.keypair)
            .expect("Noise key generation failed");

        noise::NoiseConfig::xx(dh_keys).into_authenticated()
    };

    let multiplex_config = {
        let mplex_config = mplex::MplexConfig::default();

        let mut yamux_config = yamux::YamuxConfig::default();
        yamux_config.set_max_buffer_size(MAX_RESPONSE_SIZE);
        libp2p::core::upgrade::SelectUpgrade::new(yamux_config, mplex_config)
    };

    let fuel_upgrade = FuelUpgrade::new(p2p_config.checksum);
    let connection_state = ConnectionState::new();

    let transport = if p2p_config.reserved_nodes_only_mode {
        let guarded_node = GuardedNode::new(&p2p_config.reserved_nodes);

        let fuel_authenticated =
            FuelAuthenticated::new(noise_authenticated, guarded_node);

        transport
            .authenticate(fuel_authenticated)
            .apply(fuel_upgrade)
            .multiplex(multiplex_config)
            .timeout(TRANSPORT_TIMEOUT)
            .boxed()
    } else {
        let connection_tracker =
            ConnectionTracker::new(&p2p_config.reserved_nodes, connection_state.clone());

        let fuel_authenticated =
            FuelAuthenticated::new(noise_authenticated, connection_tracker);

        transport
            .authenticate(fuel_authenticated)
            .apply(fuel_upgrade)
            .multiplex(multiplex_config)
            .timeout(TRANSPORT_TIMEOUT)
            .boxed()
    };

    (transport, connection_state)
}

pub fn peer_ids_set_from(multiaddr: &[Multiaddr]) -> HashSet<PeerId> {
    multiaddr
        .iter()
        // Safety: as is the case with `bootstrap_nodes` it is assumed that `reserved_nodes` [`Multiadr`]
        // come with PeerId included, in case they are not the `unwrap()` will only panic when the node is started.
        .map(|address| PeerId::try_from_multiaddr(address).unwrap())
        .collect()
}
