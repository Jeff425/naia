//! # Naia Shared
//! Common functionality shared between naia-server & naia-client crates.

#![deny(
    trivial_numeric_casts,
    unsafe_code,
    unstable_features,
    unused_import_braces
)]

mod ack_manager;
mod connection;
mod connection_config;
mod diff_mask;
mod entity_action_type;
mod entity_type;
mod replicate;
mod key_store;
mod keys;
mod manager_type;
mod manifest;
mod message_manager;
mod message_packet_writer;
mod packet_notifiable;
mod packet_type;
mod property_mutate;
mod protocol_type;
mod replica_builder;
mod sequence_buffer;
mod shared_config;
mod standard_header;
mod world_type;
mod wrapping_number;
mod property;

/// Commonly used utility methods to be used by naia-server & naia-client
pub mod utils;

pub use naia_socket_shared::{
    Instant, LinkConditionerConfig, PacketReader, Random, SocketConfig, Timer, Timestamp,
};

pub use ack_manager::AckManager;
pub use connection::Connection;
pub use connection_config::ConnectionConfig;
pub use diff_mask::DiffMask;
pub use entity_action_type::EntityActionType;
pub use entity_type::EntityType;
pub use property::Property;
pub use replicate::{ReplicateEq, Replicate};
pub use protocol_type::ProtocolType;
pub use key_store::KeyGenerator;
pub use keys::{LocalComponentKey, LocalEntity, NaiaKey};
pub use manager_type::ManagerType;
pub use manifest::Manifest;
pub use message_manager::MessageManager;
pub use message_packet_writer::{MessagePacketWriter, MTU_SIZE};
pub use packet_notifiable::PacketNotifiable;
pub use packet_type::PacketType;
pub use property_mutate::{PropertyMutate, PropertyMutator};
pub use protocol_type::{DynMut, DynRef, ProtocolExtractor, ProtocolKindType};
pub use replica_builder::ReplicaBuilder;
pub use sequence_buffer::{SequenceBuffer, SequenceIterator, SequenceNumber};
pub use shared_config::SharedConfig;
pub use standard_header::StandardHeader;
pub use world_type::{WorldMutType, WorldRefType};
pub use wrapping_number::{sequence_greater_than, sequence_less_than, wrapping_diff};
