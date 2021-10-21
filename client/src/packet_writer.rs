use byteorder::{BigEndian, WriteBytesExt};

use naia_shared::{
    wrapping_diff, EntityType, ManagerType, Manifest, MessagePacketWriter, NaiaKey, ProtocolType,
    Ref, Replicate, MTU_SIZE,
};

use super::{command_receiver::CommandReceiver, entity_manager::EntityManager, event::OwnedEntity};

const MAX_PAST_COMMANDS: u8 = 3;

/// Handles writing of Message/Command data into an outgoing packet
pub struct PacketWriter {
    command_working_bytes: Vec<u8>,
    command_count: u8,
    message_writer: MessagePacketWriter,
}

impl PacketWriter {
    /// Construct a new instance of `PacketReader`, the given `buffer` will be
    /// used to read information from.
    pub fn new() -> PacketWriter {
        PacketWriter {
            command_working_bytes: Vec::<u8>::new(),
            command_count: 0,
            message_writer: MessagePacketWriter::new(),
        }
    }

    /// Returns whether the writer has bytes to write into the outgoing packet
    pub fn has_bytes(&self) -> bool {
        return self.command_count != 0 || self.message_writer.has_bytes();
    }

    /// Gets the bytes to write into an outgoing packet
    pub fn get_bytes(&mut self) -> Box<[u8]> {
        let mut out_bytes = Vec::<u8>::new();

        //Write manager "header" (manager type & command count)
        if self.command_count != 0 {
            out_bytes.write_u8(ManagerType::Command as u8).unwrap(); // write manager type
            out_bytes.write_u8(self.command_count).unwrap(); // write number of commands in the following message
            out_bytes.append(&mut self.command_working_bytes); // write command payload
            self.command_count = 0;
        }

        self.message_writer.get_bytes(&mut out_bytes);

        out_bytes.into_boxed_slice()
    }

    /// Get the number of bytes which is ready to be written into an outgoing
    /// packet
    pub fn bytes_number(&self) -> usize {
        return self.command_working_bytes.len() + self.message_writer.bytes_number();
    }

    /// Writes a Command into the Writer's internal buffer, which will
    /// eventually be put into the outgoing packet
    pub fn write_command<P: ProtocolType, K: EntityType>(
        &mut self,
        host_tick: u16,
        manifest: &Manifest<P>,
        entity_manager: &EntityManager<P, K>,
        command_receiver: &CommandReceiver<P, K>,
        owned_entity: &OwnedEntity<K>,
        command: &Ref<dyn Replicate<P>>,
    ) -> bool {
        let world_entity = owned_entity.confirmed;
        if let Some(local_entity) = entity_manager.world_to_local_entity(&world_entity) {
            //Write command payload
            let mut command_payload_bytes = Vec::<u8>::new();

            command.borrow().write(&mut command_payload_bytes);

            // write past commands
            let past_commands_number = command_receiver
                .command_history_count(&world_entity)
                .min(MAX_PAST_COMMANDS);
            let mut past_command_index: u8 = 0;

            if let Some(mut iter) = command_receiver.command_history_iter(&world_entity, true) {
                while past_command_index < past_commands_number {
                    if let Some((past_tick, past_command)) = iter.next() {
                        // get tick diff between commands
                        let diff_i8: i16 = wrapping_diff(past_tick, host_tick);
                        if diff_i8 > 0 && diff_i8 <= 255 {
                            // write the tick diff
                            command_payload_bytes.write_u8(diff_i8 as u8).unwrap();
                            // write the command payload
                            past_command.borrow().write(&mut command_payload_bytes);

                            past_command_index += 1;
                        }
                    } else {
                        break;
                    }
                }
            }

            //Write command "header"
            let mut command_total_bytes = Vec::<u8>::new();

            command_total_bytes
                .write_u16::<BigEndian>(local_entity.to_u16())
                .unwrap(); // write local entity

            let type_id = command.borrow().get_type_id();
            let naia_id = manifest.get_naia_id(&type_id); // get naia id
            command_total_bytes.write_u16::<BigEndian>(naia_id).unwrap(); // write naia id
            command_total_bytes.write_u8(past_command_index).unwrap(); // write past command number
            command_total_bytes.append(&mut command_payload_bytes); // write payload

            let mut hypothetical_next_payload_size =
                self.bytes_number() + command_total_bytes.len();
            if self.command_count == 0 {
                hypothetical_next_payload_size += 2;
            }
            if hypothetical_next_payload_size < MTU_SIZE {
                self.command_count += 1;
                self.command_working_bytes.append(&mut command_total_bytes);
                return true;
            } else {
                return false;
            }
        }
        return true;
    }

    /// Writes a Message into the Writer's internal buffer, which will
    /// eventually be put into the outgoing packet
    pub fn write_message<P: ProtocolType>(
        &mut self,
        manifest: &Manifest<P>,
        message: &Ref<dyn Replicate<P>>,
    ) -> bool {
        return self.message_writer.write_message(manifest, message);
    }
}