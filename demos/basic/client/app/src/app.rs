use log::info;

use naia_client::{Client as NaiaClient, ClientConfig, Event, ProtocolType};

use naia_default_world::{Entity, World as DefaultWorld};

use naia_basic_demo_shared::{
    get_server_address, get_shared_config,
    protocol::{Auth, Character, Protocol, StringMessage},
};

type World = DefaultWorld<Protocol>;
type Client = NaiaClient<Protocol, Entity>;

pub struct App {
    client: Client,
    world: World,
    message_count: u32,
}

impl App {
    pub fn new() -> Self {
        info!("Basic Naia Client Demo started");

        let server_address = get_server_address();
        let auth = Some(Auth::new("charlie", "12345"));

        let mut client = Client::new(ClientConfig::default(), get_shared_config());
        client.connect(server_address, auth);

        App {
            client,
            world: World::new(),
            message_count: 0,
        }
    }

    pub fn update(&mut self) {
        for event in self.client.receive(&mut self.world.proxy_mut()) {
            match event {
                Ok(Event::Connection) => {
                    info!("Client connected to: {}", self.client.server_address());
                }
                Ok(Event::Disconnection) => {
                    info!("Client disconnected from: {}", self.client.server_address());
                }
                Ok(Event::Message(Protocol::StringMessage(message_ref))) => {
                    let message = message_ref.borrow();
                    let message_contents = message.contents.get();
                    info!("Client recv <- {}", message_contents);

                    let new_message_contents = format!("Client Message ({})", self.message_count);
                    info!("Client send -> {}", new_message_contents);

                    let string_message = StringMessage::new(new_message_contents);
                    self.client.queue_message(&string_message, true);
                    self.message_count += 1;
                }
                Ok(Event::SpawnEntity(entity, _)) => {
                    if let Some(character_ref) = self
                        .client
                        .entity(self.world.proxy(), &entity)
                        .component::<Character>()
                    {
                        let character = character_ref.borrow();
                        info!(
                            "creation of Character - x: {}, y: {}, name: {} {}",
                            character.x.get(),
                            character.y.get(),
                            character.fullname.get().first,
                            character.fullname.get().last,
                        );
                    }
                }
                Ok(Event::UpdateComponent(entity, _)) => {
                    if let Some(character_ref) = self
                        .client
                        .entity(self.world.proxy(), &entity)
                        .component::<Character>()
                    {
                        let character = character_ref.borrow();
                        info!(
                            "update of Character - x: {}, y: {}, name: {} {}",
                            character.x.get(),
                            character.y.get(),
                            character.fullname.get().first,
                            character.fullname.get().last,
                        );
                    }
                }
                Ok(Event::RemoveComponent(_, component_protocol)) => {
                    if let Some(character_ref) = component_protocol.to_typed_ref::<Character>() {
                        let character = character_ref.borrow();
                        info!(
                            "data delete of Character - x: {}, y: {}, name: {} {}",
                            character.x.get(),
                            character.y.get(),
                            character.fullname.get().first,
                            character.fullname.get().last,
                        );
                    }
                }
                Ok(Event::DespawnEntity(_)) => {
                    info!("deletion of Character entity");
                }
                Ok(Event::Tick) => {
                    //info!("tick event");
                }

                Err(err) => {
                    info!("Client Error: {}", err);
                    return;
                }
                _ => {}
            }
        }
    }
}