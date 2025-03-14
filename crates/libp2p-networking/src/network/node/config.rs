use crate::network::NetworkNodeType;
use libp2p::{identity::Keypair, Multiaddr};
use libp2p_identity::PeerId;
use std::{collections::HashSet, num::NonZeroUsize, time::Duration};

/// replication factor for kademlia
pub const DEFAULT_REPLICATION_FACTOR: Option<NonZeroUsize> = NonZeroUsize::new(20);

/// describe the configuration of the network
#[derive(Clone, Default, derive_builder::Builder, custom_debug::Debug)]
pub struct NetworkNodeConfig {
    #[builder(default)]
    /// The type of node (bootstrap etc)
    pub node_type: NetworkNodeType,
    /// optional identity
    #[builder(setter(into, strip_option), default)]
    #[debug(skip)]
    pub identity: Option<Keypair>,
    /// address to bind to
    #[builder(default)]
    pub bound_addr: Option<Multiaddr>,
    /// replication factor for entries in the DHT
    /// default is [`libp2p::kad::K_VALUE`] which is 20
    #[builder(setter(into, strip_option), default = "DEFAULT_REPLICATION_FACTOR")]
    pub replication_factor: Option<NonZeroUsize>,

    /// location of the dht cache, default is None
    #[builder(default = "None")]
    pub dht_cache_location: Option<String>,

    #[builder(default)]
    /// parameters for gossipsub mesh network
    pub mesh_params: Option<MeshParams>,

    /// list of addresses to connect to at initialization
    pub to_connect_addrs: HashSet<(Option<PeerId>, Multiaddr)>,
    /// republication interval in DHT, must be much less than `ttl`
    #[builder(default)]
    pub republication_interval: Option<Duration>,
    /// expiratiry for records in DHT
    #[builder(default)]
    pub ttl: Option<Duration>,
}

/// NOTE: `mesh_outbound_min <= mesh_n_low <= mesh_n <= mesh_n_high`
/// NOTE: `mesh_outbound_min <= self.config.mesh_n / 2`
/// parameters fed into gossipsub controlling the structure of the mesh
#[derive(Clone, Debug)]
pub struct MeshParams {
    /// mesh_n_high from gossipsub
    pub mesh_n_high: usize,
    /// mesh_n_low from gossipsub
    pub mesh_n_low: usize,
    /// mesh_outbound_min from gossipsub
    pub mesh_outbound_min: usize,
    /// mesh_n from gossipsub
    pub mesh_n: usize,
}

impl Default for MeshParams {
    fn default() -> Self {
        Self {
            mesh_n_high: 15,
            mesh_n_low: 8,
            mesh_outbound_min: 4,
            mesh_n: 12,
        }
    }
}
