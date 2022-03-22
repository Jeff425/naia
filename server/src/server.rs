use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    net::SocketAddr,
    panic,
    sync::{Arc, RwLock},
};

use naia_server_socket::{ServerAddrs, Socket};
use naia_shared::{serde::{BitWriter, Serde}, EntityHandle, EntityHandleConverter, ChannelIndex};
pub use naia_shared::{
    wrapping_diff, BaseConnection, BigMap, ConnectionConfig, Instant, KeyGenerator, Manifest,
    NetEntity, PacketType, PingConfig, PropertyMutate, PropertyMutator, ProtocolKindType,
    Protocolize, Replicate, ReplicateSafe, SharedConfig, StandardHeader, Timer, Timestamp,
    WorldMutType, WorldRefType,
};

use super::{
    connection::Connection,
    entity_ref::{EntityMut, EntityRef},
    entity_scope_map::EntityScopeMap,
    error::NaiaServerError,
    event::Event,
    global_diff_handler::GlobalDiffHandler,
    handshake_manager::{HandshakeManager, HandshakeResult},
    io::Io,
    room::{Room, RoomKey, RoomMut, RoomRef},
    server_config::ServerConfig,
    tick_manager::TickManager,
    user::{User, UserKey, UserMut, UserRef},
    user_scope::UserScopeMut,
    world_record::WorldRecord,
};

/// A server that uses either UDP or WebRTC communication to send/receive
/// messages to/from connected clients, and syncs registered entities to
/// clients to whom they are in-scope
pub struct Server<P: Protocolize, E: Copy + Eq + Hash, C: ChannelIndex> {
    // Config
    server_config: ServerConfig,
    shared_config: SharedConfig<P, C>,
    socket: Socket,
    io: Io,
    heartbeat_timer: Timer,
    handshake_manager: HandshakeManager<P>,
    // Users
    users: BigMap<UserKey, User>,
    user_connections: HashMap<SocketAddr, Connection<P, E>>,
    // Rooms
    rooms: BigMap<RoomKey, Room<E>>,
    // Entities
    world_record: WorldRecord<E, P::Kind>,
    entity_scope_map: EntityScopeMap<E>,
    // Components
    diff_handler: Arc<RwLock<GlobalDiffHandler<E, P::Kind>>>,
    // Events
    incoming_events: VecDeque<Result<Event<P>, NaiaServerError>>,
    // Ticks
    tick_manager: Option<TickManager>,
}

impl<P: Protocolize, E: Copy + Eq + Hash, C: ChannelIndex> Server<P, E, C> {
    /// Create a new Server
    pub fn new(server_config: &ServerConfig, shared_config: &SharedConfig<P, C>) -> Self {
        let socket = Socket::new(&shared_config.socket);

        let heartbeat_timer = Timer::new(server_config.connection.heartbeat_interval);

        let tick_manager = {
            if let Some(duration) = shared_config.tick_interval {
                Some(TickManager::new(duration))
            } else {
                None
            }
        };

        Server {
            // Config
            server_config: server_config.clone(),
            shared_config: shared_config.clone(),
            // Connection
            socket,
            io: Io::new(
                &server_config.connection.bandwidth_measure_duration,
                &shared_config.compression,
            ),
            heartbeat_timer,
            handshake_manager: HandshakeManager::new(server_config.require_auth),
            // Users
            users: BigMap::new(),
            user_connections: HashMap::new(),
            // Rooms
            rooms: BigMap::new(),
            // Entities
            world_record: WorldRecord::new(),
            entity_scope_map: EntityScopeMap::new(),
            // Components
            diff_handler: Arc::new(RwLock::new(GlobalDiffHandler::new())),
            // Events
            incoming_events: VecDeque::new(),
            // Ticks
            tick_manager,
        }
    }

    /// Listen at the given addresses
    pub fn listen(&mut self, server_addrs: &ServerAddrs) {
        self.socket.listen(server_addrs);
        self.io
            .load(self.socket.packet_sender(), self.socket.packet_receiver());
    }

    /// Returns whether or not the Server has initialized correctly and is
    /// listening for Clients
    pub fn is_listening(&self) -> bool {
        self.io.is_loaded()
    }

    /// Must be called regularly, maintains connection to and receives messages
    /// from all Clients
    pub fn receive(&mut self) -> VecDeque<Result<Event<P>, NaiaServerError>> {
        // Need to run this to maintain connection with all clients, and receive packets
        // until none left
        self.maintain_socket();

        // tick event
        let mut did_tick = false;
        if let Some(tick_manager) = &mut self.tick_manager {
            if tick_manager.recv_server_tick() {
                did_tick = true;
            }
        }

        // loop through all connections, receive Messages
        let mut user_addresses: Vec<SocketAddr> =
            self.user_connections.keys().map(|addr| *addr).collect();
        fastrand::shuffle(&mut user_addresses);

        for user_address in &user_addresses {
            let connection = self.user_connections.get_mut(user_address).unwrap();

            // receive messages from anyone
            while let Some(message) = connection.base.message_manager.pop_incoming_message() {
                self.incoming_events
                    .push_back(Ok(Event::Message(connection.user_key, message)));
            }

            // receive entity messages on tick
            if did_tick {
                // Receive EntityMessages
                for user_address in &user_addresses {
                    let connection = self.user_connections.get_mut(user_address).unwrap();

                    while let Some(message) = connection.pop_incoming_entity_message(
                        self.tick_manager.as_ref().unwrap().server_tick(),
                    ) {
                        self.incoming_events.push_back(Ok(Event::Message(
                            connection.user_key,
                            message,
                        )));
                    }
                }

                self.incoming_events.push_back(Ok(Event::Tick));
            }
        }

        // tick event
        if did_tick {
            self.incoming_events.push_back(Ok(Event::Tick));
        }

        return std::mem::take(&mut self.incoming_events);
    }

    // Connections

    /// Accepts an incoming Client User, allowing them to establish a connection
    /// with the Server
    pub fn accept_connection(&mut self, user_key: &UserKey) {
        if let Some(user) = self.users.get(user_key) {
            let mut new_connection = Connection::new(
                &self.server_config.connection,
                user.address,
                &user_key,
                &self.diff_handler,
            );
            // send connectaccept response
            let mut writer = self
                .handshake_manager
                .write_connect_response(&mut new_connection);
            self.io.send_writer(&user.address, &mut writer);
            //
            self.user_connections.insert(user.address, new_connection);
            if self.io.bandwidth_monitor_enabled() {
                self.io.register_client(&user.address);
            }
            self.incoming_events
                .push_back(Ok(Event::Connection(*user_key)));
        }
    }

    /// Rejects an incoming Client User, terminating their attempt to establish
    /// a connection with the Server
    pub fn reject_connection(&mut self, user_key: &UserKey) {
        self.delete_user(user_key);
    }

    // Messages

    /// Queues up an Message to be sent to the Client associated with a given
    /// UserKey
    pub fn send_message<R: ReplicateSafe<P>>(
        &mut self,
        user_key: &UserKey,
        message: &R,
        guaranteed_delivery: bool,
    ) {
        if let Some(user) = self.users.get(user_key) {
            if let Some(connection) = self.user_connections.get_mut(&user.address) {
                if message.has_entity_properties() {
                    // collect all entities in the message
                    let entities: Vec<E> = message
                        .entities()
                        .iter()
                        .map(|handle| self.world_record.handle_to_entity(&handle))
                        .collect();

                    // check whether all entities are in scope for the connection
                    let all_entities_in_scope = {
                        entities.iter().all(|entity| {
                            connection.entity_manager.entity_in_scope(entity)
                        })
                    };
                    if all_entities_in_scope {
                        // All necessary entities are in scope, so send message
                        connection
                            .base
                            .message_manager
                            .send_message(message, true);
                    } else {
                        // Entity hasn't been added to the User Scope yet, or replicated to Client yet
                        connection
                            .entity_manager
                            .queue_entity_message(entities, message);
                    }
                } else {
                    connection
                        .base
                        .message_manager
                        .send_message(message, guaranteed_delivery);
                }
            }
        }
    }

    // Updates

    /// Used to evaluate whether, given a User & Entity that are in the
    /// same Room, said Entity should be in scope for the given User.
    ///
    /// While Rooms allow for a very simple scope to which an Entity can belong,
    /// this provides complete customization for advanced scopes.
    ///
    /// Return a collection of Entity Scope Sets, being a unique combination of
    /// a related Room, User, and Entity, used to determine which Entities to
    /// replicate to which Users
    pub fn scope_checks(&self) -> Vec<(RoomKey, UserKey, E)> {
        let mut list: Vec<(RoomKey, UserKey, E)> = Vec::new();

        // TODO: precache this, instead of generating a new list every call
        // likely this is called A LOT
        for (room_key, room) in self.rooms.iter() {
            for user_key in room.user_keys() {
                for entity in room.entities() {
                    list.push((room_key, *user_key, *entity));
                }
            }
        }

        return list;
    }

    /// Sends all update messages to all Clients. If you don't call this
    /// method, the Server will never communicate with it's connected
    /// Clients
    pub fn send_all_updates<W: WorldRefType<P, E>>(&mut self, world: W) {
        // update entity scopes
        self.update_entity_scopes(&world);

        // loop through all connections, send packet
        let mut user_addresses: Vec<SocketAddr> =
            self.user_connections.keys().map(|addr| *addr).collect();
        fastrand::shuffle(&mut user_addresses);

        for user_address in user_addresses {
            let connection = self.user_connections.get_mut(&user_address).unwrap();
            connection.entity_manager.collect_component_updates();
            connection.collect_entity_messages();
            connection.send_outgoing_packets(
                &mut self.io,
                &world,
                &self.world_record,
                &self.tick_manager,
            );
        }
    }

    // Entities

    /// Creates a new Entity and returns an EntityMut which can be used for
    /// further operations on the Entity
    pub fn spawn_entity<W: WorldMutType<P, E>>(&mut self, mut world: W) -> EntityMut<P, E, W, C> {
        let entity = world.spawn_entity();
        self.spawn_entity_init(&entity);

        return EntityMut::new(self, world, &entity);
    }

    /// Creates a new Entity with a specific id
    pub fn spawn_entity_at(&mut self, entity: &E) {
        self.spawn_entity_init(&entity);
    }

    /// Retrieves an EntityRef that exposes read-only operations for the
    /// Entity.
    /// Panics if the Entity does not exist.
    pub fn entity<W: WorldRefType<P, E>>(&self, world: W, entity: &E) -> EntityRef<P, E, W> {
        if world.has_entity(entity) {
            return EntityRef::new(world, &entity);
        }
        panic!("No Entity exists for given Key!");
    }

    /// Retrieves an EntityMut that exposes read and write operations for the
    /// Entity.
    /// Panics if the Entity does not exist.
    pub fn entity_mut<W: WorldMutType<P, E>>(
        &mut self,
        world: W,
        entity: &E,
    ) -> EntityMut<P, E, W, C> {
        if world.has_entity(entity) {
            return EntityMut::new(self, world, &entity);
        }
        panic!("No Entity exists for given Key!");
    }

    /// Gets a Vec of all Entities in the given World
    pub fn entities<W: WorldRefType<P, E>>(&self, world: W) -> Vec<E> {
        return world.entities();
    }

    // Users

    /// Returns whether or not a User exists for the given RoomKey
    pub fn user_exists(&self, user_key: &UserKey) -> bool {
        return self.users.contains_key(user_key);
    }

    /// Retrieves an UserRef that exposes read-only operations for the User
    /// associated with the given UserKey.
    /// Panics if the user does not exist.
    pub fn user(&self, user_key: &UserKey) -> UserRef<P, E, C> {
        if self.users.contains_key(user_key) {
            return UserRef::new(self, &user_key);
        }
        panic!("No User exists for given Key!");
    }

    /// Retrieves an UserMut that exposes read and write operations for the User
    /// associated with the given UserKey.
    /// Returns None if the user does not exist.
    pub fn user_mut(&mut self, user_key: &UserKey) -> UserMut<P, E, C> {
        if self.users.contains_key(user_key) {
            return UserMut::new(self, &user_key);
        }
        panic!("No User exists for given Key!");
    }

    /// Return a list of all currently connected Users' keys
    pub fn user_keys(&self) -> Vec<UserKey> {
        let mut output = Vec::new();

        for (user_key, _) in self.users.iter() {
            output.push(user_key);
        }

        return output;
    }

    /// Get the number of Users currently connected
    pub fn users_count(&self) -> usize {
        return self.users.len();
    }

    /// Returns a UserScopeMut, which is used to include/exclude Entities for a
    /// given User
    pub fn user_scope(&mut self, user_key: &UserKey) -> UserScopeMut<P, E, C> {
        if self.users.contains_key(user_key) {
            return UserScopeMut::new(self, &user_key);
        }
        panic!("No User exists for given Key!");
    }

    /// Returns whether a given User has a particular Entity in-scope currently
    pub fn user_scope_has_entity(&self, user_key: &UserKey, entity: &E) -> bool {
        if let Some(user) = self.users.get(user_key) {
            if let Some(user_connection) = self.user_connections.get(&user.address) {
                return user_connection.entity_manager.has_entity(entity);
            }
        }

        return false;
    }

    // Rooms

    /// Creates a new Room on the Server and returns a corresponding RoomMut,
    /// which can be used to add users/entities to the room or retrieve its
    /// key
    pub fn make_room(&mut self) -> RoomMut<P, E, C> {
        let new_room = Room::new();
        let room_key = self.rooms.insert(new_room);
        return RoomMut::new(self, &room_key);
    }

    /// Returns whether or not a Room exists for the given RoomKey
    pub fn room_exists(&self, room_key: &RoomKey) -> bool {
        return self.rooms.contains_key(room_key);
    }

    /// Retrieves an RoomMut that exposes read and write operations for the
    /// Room associated with the given RoomKey.
    /// Panics if the room does not exist.
    pub fn room(&self, room_key: &RoomKey) -> RoomRef<P, E, C> {
        if self.rooms.contains_key(room_key) {
            return RoomRef::new(self, room_key);
        }
        panic!("No Room exists for given Key!");
    }

    /// Retrieves an RoomMut that exposes read and write operations for the
    /// Room associated with the given RoomKey.
    /// Panics if the room does not exist.
    pub fn room_mut(&mut self, room_key: &RoomKey) -> RoomMut<P, E, C> {
        if self.rooms.contains_key(room_key) {
            return RoomMut::new(self, room_key);
        }
        panic!("No Room exists for given Key!");
    }

    /// Return a list of all the Server's Rooms' keys
    pub fn room_keys(&self) -> Vec<RoomKey> {
        let mut output = Vec::new();

        for (key, _) in self.rooms.iter() {
            output.push(key);
        }

        return output;
    }

    /// Get a count of how many Rooms currently exist
    pub fn rooms_count(&self) -> usize {
        self.rooms.len()
    }

    // Ticks

    /// Gets the last received tick from the Client
    pub fn client_tick(&self, user_key: &UserKey) -> Option<u16> {
        if let Some(user) = self.users.get(user_key) {
            if let Some(user_connection) = self.user_connections.get(&user.address) {
                return Some(user_connection.last_received_tick);
            }
        }
        return None;
    }

    /// Gets the current tick of the Server
    pub fn server_tick(&self) -> Option<u16> {
        return self
            .tick_manager
            .as_ref()
            .map(|tick_manager| tick_manager.server_tick());
    }

    // Bandwidth monitoring
    pub fn outgoing_bandwidth_total(&mut self) -> f32 {
        return self.io.outgoing_bandwidth_total();
    }

    pub fn incoming_bandwidth_total(&mut self) -> f32 {
        return self.io.incoming_bandwidth_total();
    }

    pub fn outgoing_bandwidth_to_client(&mut self, address: &SocketAddr) -> f32 {
        return self.io.outgoing_bandwidth_to_client(address);
    }

    pub fn incoming_bandwidth_from_client(&mut self, address: &SocketAddr) -> f32 {
        return self.io.incoming_bandwidth_from_client(address);
    }

    // Crate-Public methods

    //// Entities

    /// Despawns the Entity, if it exists.
    /// This will also remove all of the Entity’s Components.
    /// Returns true if the Entity is successfully despawned and false if the
    /// Entity does not exist.
    pub(crate) fn despawn_entity<W: WorldMutType<P, E>>(&mut self, world: &mut W, entity: &E) {
        if !world.has_entity(entity) {
            panic!("attempted to de-spawn nonexistent entity");
        }

        // TODO: we can make this more efficient in the future by caching which Entities
        // are in each User's scope
        for (_, user_connection) in self.user_connections.iter_mut() {
            //remove entity from user connection
            user_connection
                .entity_manager
                .despawn_entity(&self.world_record, entity);
        }

        // Clean up associated components
        for component_kind in self.world_record.component_kinds(entity) {
            self.component_cleanup(entity, &component_kind);
        }

        // Delete from world
        world.despawn_entity(entity);

        // Delete scope
        self.entity_scope_map.remove_entity(entity);

        // Remove from ECS Record
        self.world_record.despawn_entity(entity);
    }

    //// Entity Scopes

    pub(crate) fn user_scope_set_entity(
        &mut self,
        user_key: &UserKey,
        entity: &E,
        is_contained: bool,
    ) {
        self.entity_scope_map
            .insert(*user_key, *entity, is_contained);
    }

    //// Components

    /// Adds a Component to an Entity
    pub(crate) fn insert_component<R: ReplicateSafe<P>, W: WorldMutType<P, E>>(
        &mut self,
        world: &mut W,
        entity: &E,
        mut component_ref: R,
    ) {
        if !world.has_entity(entity) {
            panic!("attempted to add component to non-existent entity");
        }

        let component_kind = component_ref.kind();

        if world.has_component_of_kind(entity, &component_kind) {
            panic!(
                "attempted to add component to entity which already has one of that type! \
                   an entity is not allowed to have more than 1 type of component at a time."
            )
        }

        self.component_init(entity, &mut component_ref);

        // actually insert component into world
        world.insert_component(entity, component_ref);

        // add component to connections already tracking entity
        for (_, user_connection) in self.user_connections.iter_mut() {
            if user_connection.entity_manager.has_entity(entity) {
                // insert component into user's connection
                user_connection
                    .entity_manager
                    .insert_component(entity, &component_kind);
            }
        }
    }

    /// Removes a Component from an Entity
    pub(crate) fn remove_component<R: Replicate<P>, W: WorldMutType<P, E>>(
        &mut self,
        world: &mut W,
        entity: &E,
    ) -> Option<R> {
        // get component key from type
        let component_kind = P::kind_of::<R>();

        // clean up component on all connections
        // TODO: should be able to make this more efficient by caching for every Entity
        // which scopes they are part of
        for (_, user_connection) in self.user_connections.iter_mut() {
            //remove component from user connection
            user_connection
                .entity_manager
                .remove_component(entity, &component_kind);
        }

        // cleanup all other loose ends
        self.component_cleanup(entity, &component_kind);

        // remove from world
        return world.remove_component::<R>(entity);
    }

    //// Users

    /// Get a User's Socket Address, given the associated UserKey
    pub(crate) fn user_address(&self, user_key: &UserKey) -> Option<SocketAddr> {
        if let Some(user) = self.users.get(user_key) {
            return Some(user.address);
        }
        return None;
    }

    /// All necessary cleanup, when they're actually gone...
    pub(crate) fn delete_user(&mut self, user_key: &UserKey) -> Option<User> {
        if let Some(user) = self.users.remove(user_key) {
            if let Some(_) = self.user_connections.remove(&user.address) {
                self.entity_scope_map.remove_user(user_key);
                self.handshake_manager.delete_user(&user.address);

                // TODO: cache this?
                // Clean up all user data
                for (_, room) in self.rooms.iter_mut() {
                    room.unsubscribe_user(&user_key);
                }

                if self.io.bandwidth_monitor_enabled() {
                    self.io.deregister_client(&user.address);
                }

                return Some(user);
            }
        }

        return None;
    }

    //// Rooms

    /// Deletes the Room associated with a given RoomKey on the Server.
    /// Returns true if the Room existed.
    pub(crate) fn room_destroy(&mut self, room_key: &RoomKey) -> bool {
        self.room_remove_all_entities(room_key);

        if self.rooms.contains_key(room_key) {
            // TODO: what else kind of cleanup do we need to do here? Scopes?

            // actually remove the room from the collection
            self.rooms.remove(room_key);

            return true;
        } else {
            return false;
        }
    }

    //////// users

    /// Returns whether or not an User is currently in a specific Room, given
    /// their keys.
    pub(crate) fn room_has_user(&self, room_key: &RoomKey, user_key: &UserKey) -> bool {
        if let Some(room) = self.rooms.get(room_key) {
            return room.has_user(user_key);
        }
        return false;
    }

    /// Add an User to a Room, given the appropriate RoomKey & UserKey
    /// Entities will only ever be in-scope for Users which are in a
    /// Room with them
    pub(crate) fn room_add_user(&mut self, room_key: &RoomKey, user_key: &UserKey) {
        if let Some(room) = self.rooms.get_mut(room_key) {
            room.subscribe_user(user_key);
        }
    }

    /// Removes a User from a Room
    pub(crate) fn room_remove_user(&mut self, room_key: &RoomKey, user_key: &UserKey) {
        if let Some(room) = self.rooms.get_mut(room_key) {
            room.unsubscribe_user(user_key);
        }
    }

    /// Get a count of Users in a given Room
    pub(crate) fn room_users_count(&self, room_key: &RoomKey) -> usize {
        if let Some(room) = self.rooms.get(room_key) {
            return room.users_count();
        }
        return 0;
    }

    //////// entities

    /// Returns whether or not an Entity is currently in a specific Room, given
    /// their keys.
    pub(crate) fn room_has_entity(&self, room_key: &RoomKey, entity: &E) -> bool {
        return self.world_record.entity_is_in_room(entity, room_key);
    }

    /// Add an Entity to a Room associated with the given RoomKey.
    /// Entities will only ever be in-scope for Users which are in a Room with
    /// them.
    pub(crate) fn room_add_entity(&mut self, room_key: &RoomKey, entity: &E) {
        let mut is_some = false;
        if let Some(room) = self.rooms.get_mut(room_key) {
            room.add_entity(entity);
            is_some = true;
        }
        if is_some {
            self.world_record.entity_enter_room(entity, room_key);
        }
    }

    /// Remove an Entity from a Room, associated with the given RoomKey
    pub(crate) fn room_remove_entity(&mut self, room_key: &RoomKey, entity: &E) {
        if let Some(room) = self.rooms.get_mut(room_key) {
            room.remove_entity(entity);
            self.world_record.entity_leave_rooms(entity);
        }
    }

    /// Remove all Entities from a Room, associated with the given RoomKey
    fn room_remove_all_entities(&mut self, room_key: &RoomKey) {
        if let Some(room) = self.rooms.get_mut(room_key) {

            let entities: Vec<E> = room.entities().map(|entity_ref| *entity_ref).collect();
            for entity in entities {
                room.remove_entity(&entity);
                self.world_record.entity_leave_rooms(&entity);
            }
        }
    }

    /// Get a count of Entities in a given Room
    pub(crate) fn room_entities_count(&self, room_key: &RoomKey) -> usize {
        if let Some(room) = self.rooms.get(room_key) {
            return room.entities_count();
        }
        return 0;
    }

    // Private methods

    fn maintain_socket(&mut self) {
        // heartbeats & disconnects
        if self.heartbeat_timer.ringing() {
            self.heartbeat_timer.reset();

            let mut user_disconnects: Vec<UserKey> = Vec::new();

            for (user_address, connection) in &mut self.user_connections.iter_mut() {
                if connection.base.should_drop() {
                    user_disconnects.push(connection.user_key);
                    continue;
                }

                if connection.base.should_send_heartbeat() {
                    // Don't try to refactor this to self.internal_send, doesn't seem to
                    // work cause of iter_mut()
                    let mut writer = BitWriter::new();

                    // write header
                    connection
                        .base
                        .write_outgoing_header(PacketType::Heartbeat, &mut writer);

                    // write server tick
                    if let Some(tick_manager) = self.tick_manager.as_mut() {
                        tick_manager.write_server_tick(&mut writer);
                    }

                    // send packet
                    self.io.send_writer(user_address, &mut writer);
                    connection.base.mark_sent();
                }
            }

            for user_key in user_disconnects {
                self.disconnect_user(&user_key);
            }
        }

        //receive socket events
        loop {
            match self.io.recv_reader() {
                Ok(Some((address, owned_reader))) => {
                    let mut reader = owned_reader.borrow();

                    // Read header
                    let header = StandardHeader::de(&mut reader).unwrap();

                    if let Some(user_connection) = self.user_connections.get_mut(&address) {
                        // Mark that we've heard from the client
                        user_connection.base.mark_heard();

                        // read client tick
                        if let Some(tick_manager) = self.tick_manager.as_ref() {
                            let client_tick = tick_manager.read_client_tick(&mut reader);
                            user_connection.recv_client_tick(client_tick);
                        }
                    }

                    match header.packet_type() {
                        PacketType::ClientChallengeRequest => {
                            let mut writer =
                                self.handshake_manager.recv_challenge_request(&mut reader);
                            self.io.send_writer(&address, &mut writer);
                        }
                        PacketType::ClientConnectRequest => {
                            if let Some(mut connection) = self.user_connections.get_mut(&address) {
                                self.handshake_manager.recv_old_connect_request(
                                    &mut self.io,
                                    &self.world_record,
                                    &mut connection,
                                    &header,
                                    &mut reader,
                                );
                            } else {
                                match self.handshake_manager.recv_new_connect_request(
                                    &self.shared_config.manifest,
                                    &address,
                                    &mut reader,
                                ) {
                                    HandshakeResult::AuthUser(auth_message) => {
                                        let user = User::new(address);
                                        let user_key = self.users.insert(user);
                                        self.incoming_events.push_back(Ok(Event::Authorization(
                                            user_key,
                                            auth_message,
                                        )));
                                    }
                                    HandshakeResult::ConnectUser => {
                                        let user = User::new(address);
                                        let user_key = self.users.insert(user);
                                        self.accept_connection(&user_key);
                                    }
                                    HandshakeResult::Invalid => {
                                        // do nothing
                                    }
                                }
                            }
                        }
                        PacketType::Disconnect => {
                            if let Some(user_key) = loop {
                                if let Some(mut connection) =
                                    self.user_connections.get_mut(&address)
                                {
                                    if self
                                        .handshake_manager
                                        .verify_disconnect_request(&mut connection, &mut reader)
                                    {
                                        break Some(connection.user_key);
                                    }
                                }
                                break None;
                            } {
                                self.disconnect_user(&user_key);
                            }
                        }
                        PacketType::Data => match self.user_connections.get_mut(&address) {
                            Some(connection) => {
                                connection.process_incoming_header(&self.world_record, &header);
                                connection.process_incoming_data(
                                    &self.tick_manager,
                                    &self.shared_config.manifest,
                                    &mut reader,
                                    &self.world_record
                                );
                            }
                            None => {
                                warn!("received data from unauthenticated client: {}", address);
                            }
                        },
                        PacketType::Heartbeat => {
                            match self.user_connections.get_mut(&address) {
                                Some(connection) => {
                                    // Still need to do this so that proper notify
                                    // events fire based on the heartbeat header
                                    connection.process_incoming_header(&self.world_record, &header);
                                }
                                None => {
                                    warn!(
                                        "received heartbeat from unauthenticated client: {}",
                                        address
                                    );
                                }
                            }
                        }
                        PacketType::Ping => {
                            if self.shared_config.ping.is_some() {
                                match self.user_connections.get_mut(&address) {
                                    Some(connection) => {
                                        connection
                                            .process_incoming_header(&self.world_record, &header);

                                        // read incoming ping index
                                        let ping_index = u16::de(&mut reader).unwrap();

                                        // write pong payload
                                        let mut writer = BitWriter::new();

                                        // write header
                                        connection
                                            .base
                                            .write_outgoing_header(PacketType::Pong, &mut writer);

                                        // write server tick
                                        if let Some(tick_manager) = self.tick_manager.as_ref() {
                                            tick_manager.write_server_tick(&mut writer);
                                        }

                                        // write index
                                        ping_index.ser(&mut writer);

                                        // send packet
                                        self.io.send_writer(&address, &mut writer);
                                        connection.base.mark_sent();
                                    }
                                    None => {
                                        warn!(
                                            "received ping from unauthenticated client: {}",
                                            address
                                        );
                                    }
                                }
                            } else {
                                warn!("received ping address: {}, even though not configured to receive pings", address);
                            }
                        }
                        PacketType::ServerChallengeResponse
                        | PacketType::ServerConnectResponse
                        | PacketType::Pong => {
                            // do nothing
                        }
                    }
                }
                Ok(None) => {
                    // No more packets, break loop
                    break;
                }
                Err(error) => {
                    self.incoming_events
                        .push_back(Err(NaiaServerError::Wrapped(Box::new(error))));
                }
            }
        }
    }

    pub(crate) fn disconnect_user(&mut self, user_key: &UserKey) {
        if let Some(user) = self.delete_user(user_key) {
            self.incoming_events
                .push_back(Ok(Event::Disconnection(*user_key, user)));
        }
    }

    // Entity Helpers

    fn spawn_entity_init(&mut self, entity: &E) {
        self.world_record.spawn_entity(entity);
    }

    // Entity Scopes

    fn update_entity_scopes<W: WorldRefType<P, E>>(&mut self, world: &W) {
        for (_, room) in self.rooms.iter_mut() {
            while let Some((removed_user, removed_entity)) = room.pop_entity_removal_queue() {
                if let Some(user) = self.users.get(&removed_user) {
                    if let Some(user_connection) = self.user_connections.get_mut(&user.address) {
                        //remove entity from user connection
                        user_connection
                            .entity_manager
                            .despawn_entity(&self.world_record, &removed_entity);
                    }
                }
            }

            // TODO: we should be able to cache these tuples of keys to avoid building a new
            // list each time
            for user_key in room.user_keys() {
                for entity in room.entities() {
                    if world.has_entity(entity) {
                        if let Some(user) = self.users.get(user_key) {
                            if let Some(user_connection) =
                                self.user_connections.get_mut(&user.address)
                            {
                                let currently_in_scope =
                                    user_connection.entity_manager.has_entity(entity);

                                let should_be_in_scope: bool;
                                if let Some(in_scope) = self.entity_scope_map.get(user_key, entity)
                                {
                                    should_be_in_scope = *in_scope;
                                } else {
                                    should_be_in_scope = false;
                                }

                                if should_be_in_scope {
                                    if !currently_in_scope {
                                        // add entity to the connections local scope
                                        user_connection
                                            .entity_manager
                                            .spawn_entity(&self.world_record, entity);
                                    }
                                } else {
                                    if currently_in_scope {
                                        // remove entity from the connections local scope
                                        user_connection
                                            .entity_manager
                                            .despawn_entity(&self.world_record, entity);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Component Helpers

    fn component_init<R: ReplicateSafe<P>>(&mut self, entity: &E, component_ref: &mut R) {
        let component_kind = component_ref.kind();
        self.world_record.add_component(entity, &component_kind);

        let diff_mask_length: u8 = component_ref.diff_mask_size();

        let mut_sender = self
            .diff_handler
            .as_ref()
            .write()
            .expect("DiffHandler should be initialized")
            .register_component(entity, &component_kind, diff_mask_length);

        let prop_mutator = PropertyMutator::new(mut_sender);

        component_ref.set_mutator(&prop_mutator);
    }

    fn component_cleanup(&mut self, entity: &E, component_kind: &P::Kind) {
        self.world_record.remove_component(entity, component_kind);
        self.diff_handler
            .as_ref()
            .write()
            .expect("Haven't initialized DiffHandler")
            .deregister_component(entity, component_kind);
    }
}

impl<P: Protocolize, E: Copy + Eq + Hash, C: ChannelIndex> EntityHandleConverter<E> for Server<P, E, C> {
    fn handle_to_entity(&self, entity_handle: &EntityHandle) -> E {
        return self.world_record.handle_to_entity(entity_handle);
    }

    fn entity_to_handle(&self, entity: &E) -> EntityHandle {
        return self.world_record.entity_to_handle(entity);
    }
}
