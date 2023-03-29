use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::io::Write;
use std::ops::Not;
use std::str::Utf8Error;

use lazy_static::lazy_static;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use crate::connection::ConnectionState;

#[derive(Hash, PartialEq, Eq, Copy, Clone, Debug)]
pub enum PacketType {
    HandshakeServerboundStart,
    StatusServerboundRequest,
    StatusClientboundResponse,
    StatusServerboundPing,
    StatusClientboundPong,
    LoginServerboundStart,
    LoginClientboundSuccess,
    PlayClientboundLogin,
    PlayClientboundDifficulty,
    PlayClientboundAbilities,
    PlayClientboundSetDefaultSpawnPosition
}

#[derive(Hash, PartialEq, Eq)]
struct PacketTypeKey {
    state: ConnectionState,
    id: i32,
}

lazy_static! {
    static ref SERVERBOUND_PACKET_TYPES: HashMap<PacketTypeKey, PacketType> = HashMap::from([
        (PacketTypeKey { state: ConnectionState::Handshake, id: 0x00 }, PacketType::HandshakeServerboundStart),
        (PacketTypeKey { state: ConnectionState::Status, id: 0x00 }, PacketType::StatusServerboundRequest),
        (PacketTypeKey { state: ConnectionState::Status, id: 0x01 }, PacketType::StatusServerboundPing),
        (PacketTypeKey { state: ConnectionState::Login, id: 0x00 }, PacketType::LoginServerboundStart),
    ]);

    static ref CLIENTBOUND_PACKET_TYPES: HashMap<PacketType, i32> = HashMap::from([
        (PacketType::StatusClientboundResponse, 0x00),
        (PacketType::StatusClientboundPong, 0x01),
        (PacketType::LoginClientboundSuccess, 0x02),
        (PacketType::PlayClientboundLogin, 0x28),
        (PacketType::PlayClientboundDifficulty, 0x0C),
        (PacketType::PlayClientboundAbilities, 0x34),
        (PacketType::PlayClientboundSetDefaultSpawnPosition, 0x50)
    ]);
}

#[derive(Debug)]
pub enum DecodingError {
    PacketTooSmall,
    VarIntTooBig,
    InvalidPacketId(i32, ConnectionState),
    StringTooSmall,
    StringTooLarge,
    StringInvalidUtf8(Utf8Error),
    InvalidClientboundPacket(PacketType),
}

impl Display for DecodingError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        (self as &dyn Debug).fmt(f)
    }
}

impl Error for DecodingError {}

pub struct Packet {
    pub data: Vec<u8>,
    pub raw_size: usize,
    pub packet_type: PacketType,
}

impl Packet {
    pub async fn decode(buf: &Vec<u8>, state: ConnectionState) -> Result<Packet, DecodingError> {
        let mut reader = PacketReader::create(buf);

        Self::read(&mut reader, state)
    }

    fn read(reader: &mut PacketReader, state: ConnectionState) -> Result<Packet, DecodingError> {
        let packet_beginning = reader.reader_index;

        if reader.left_to_read() < 1 {
            return Err(DecodingError::PacketTooSmall);
        }

        let length = reader.read_varint()?;

        if length > reader.left_to_read() as i32 {
            return Err(DecodingError::PacketTooSmall);
        }

        let (packet_id, packet_id_size) = reader.read_varint_with_size()?;
        let packet_type = Self::packet_id_to_type(packet_id, state)?;

        let buffer_length = (length as usize) - packet_id_size;
        let mut buffer: Vec<u8> = vec![0; buffer_length];
        reader.try_read_all(&mut buffer).expect("this should not happen");

        let packet = Packet {
            data: buffer,
            raw_size: reader.reader_index - packet_beginning,
            packet_type,
        };

        Ok(packet)
    }

    fn packet_id_to_type(id: i32, state: ConnectionState) -> Result<PacketType, DecodingError> {
        match SERVERBOUND_PACKET_TYPES.get(&PacketTypeKey { state, id }) {
            Some(packet_type) => Ok(*packet_type),
            None => Err(DecodingError::InvalidPacketId(id, state))
        }
    }

    fn packet_type_to_id(packet_type: PacketType) -> Result<i32, DecodingError> {
        match CLIENTBOUND_PACKET_TYPES.get(&packet_type) {
            Some(packet_type) => Ok(*packet_type),
            None => Err(DecodingError::InvalidClientboundPacket(packet_type))
        }
    }
}


pub struct PacketReader<'a> {
    buf: &'a Vec<u8>,
    reader_index: usize,
}

impl<'a> PacketReader<'a> {
    pub fn create(buf: &'a Vec<u8>) -> Self {
        PacketReader {
            buf,
            reader_index: 0,
        }
    }

    pub fn left_to_read(&self) -> usize { self.buf.len() - self.reader_index }

    pub fn ensure_at_least(&self, len: usize) -> Result<(), DecodingError> {
        if self.reader_index + len > self.buf.len() {
            Err(DecodingError::StringTooSmall)
        } else {
            Ok(())
        }
    }

    pub fn read_one_unsafe(&mut self) -> u8 {
        let result = self.buf[self.reader_index];
        self.reader_index += 1;
        result
    }

    pub fn try_read_one(&mut self) -> Result<u8, DecodingError> {
        if self.reader_index >= self.buf.len() {
            Err(DecodingError::PacketTooSmall)
        } else {
            Ok(self.read_one_unsafe())
        }
    }

    pub fn try_read_all(&mut self, out: &mut Vec<u8>) -> Result<(), DecodingError> {
        let len = out.len();
        self.ensure_at_least(len)?;

        out.copy_from_slice(&self.buf[self.reader_index..self.reader_index + len]);
        self.reader_index += len;

        Ok(())
    }

    pub fn read_varint(&mut self) -> Result<i32, DecodingError> {
        let mut value: i32 = 0;
        let mut position: i32 = 0;

        loop {
            let current_byte = self.try_read_one()? as i32;
            value |= (current_byte & 0x7F) << position;

            if (current_byte & 0x80) == 0 {
                break;
            }

            position += 7;

            if position >= 32 {
                return Err(DecodingError::VarIntTooBig);
            }
        }

        Ok(value)
    }

    pub fn read_varint_with_size(&mut self) -> Result<(i32, usize), DecodingError> {
        let index_before = self.reader_index;
        let varint = self.read_varint()?;

        Ok((varint, self.reader_index - index_before))
    }

    pub fn read_string(&mut self, max_length: usize) -> Result<String, DecodingError> {
        let size = self.read_varint()? as usize;
        if size > max_length {
            return Err(DecodingError::StringTooLarge);
        }

        self.ensure_at_least(size).map_err(|_| DecodingError::StringTooSmall)?;

        let slice = &self.buf[self.reader_index..self.reader_index + size];
        self.reader_index += size;

        match std::str::from_utf8(slice) {
            Ok(str) => Ok(str.to_string()),
            Err(e) => Err(DecodingError::StringInvalidUtf8(e))
        }
    }

    pub fn read_boolean(&mut self) -> Result<bool, DecodingError> {
        self.try_read_one().map(|value| value != 0)
    }

    pub fn read_short(&mut self) -> Result<u16, DecodingError> {
        self.ensure_at_least(2)?;

        let result = ((self.read_one_unsafe() as u16) << 8) |
            (self.read_one_unsafe() as u16);

        Ok(result)
    }

    pub fn read_long(&mut self) -> Result<i64, DecodingError> {
        self.ensure_at_least(2)?;

        let result = ((self.read_one_unsafe() as i64) << 56) |
            ((self.read_one_unsafe() as i64) << 48) |
            ((self.read_one_unsafe() as i64) << 40) |
            ((self.read_one_unsafe() as i64) << 32) |

            ((self.read_one_unsafe() as i64) << 24) |
            ((self.read_one_unsafe() as i64) << 16) |
            ((self.read_one_unsafe() as i64) << 8) |
            (self.read_one_unsafe() as i64);

        Ok(result)
    }

    pub fn read_uuid(&mut self) -> Result<Uuid, DecodingError> {
        Ok(Uuid::from_u64_pair(
            self.read_long()? as u64,
            self.read_long()? as u64,
        ))
    }

    pub fn read_optional<T, F>(&mut self, read: F) -> Result<Option<T>, DecodingError>
        where F: FnOnce(&mut Self) -> Result<T, DecodingError> {
        if self.read_boolean()? {
            let result = read(self)?;

            Ok(Some(result))
        } else {
            Ok(None)
        }
    }
}

pub struct PacketWriter {
    buf: Vec<u8>,
}

impl PacketWriter {
    pub fn create(capacity: usize) -> Self {
        PacketWriter {
            buf: Vec::with_capacity(capacity)
        }
    }

    pub fn write_packet_type(&mut self, packet_type: PacketType) {
        self.write_var_int(Packet::packet_type_to_id(packet_type).expect("sending invalid packet"));
    }

    pub fn write_byte(&mut self, byte: u8) {
        self.buf.push(byte)
    }

    pub fn write_boolean(&mut self, boolean: bool) {
        self.write_byte(if boolean { 1 } else { 0 });
    }

    pub fn write_int(&mut self, value: i32) {
        self.buf.reserve(4);

        self.write_byte(((value >> 24) & 0xFF) as u8);
        self.write_byte(((value >> 16) & 0xFF) as u8);
        self.write_byte(((value >> 8) & 0xFF) as u8);
        self.write_byte((value  & 0xFF) as u8);
    }

    pub fn write_long(&mut self, value: i64) {
        self.buf.reserve(8);

        self.write_byte(((value >> 56) & 0xFF) as u8);
        self.write_byte(((value >> 48) & 0xFF) as u8);
        self.write_byte(((value >> 40) & 0xFF) as u8);
        self.write_byte(((value >> 32) & 0xFF) as u8);

        self.write_byte(((value >> 24) & 0xFF) as u8);
        self.write_byte(((value >> 16) & 0xFF) as u8);
        self.write_byte(((value >> 8) & 0xFF) as u8);
        self.write_byte((value & 0xFF) as u8);
    }

    pub fn write_float(&mut self, value: f32) {
        self.write_all(value.to_be_bytes().as_ref()).unwrap();
    }

    pub fn write_position(&mut self, x: i32, y: i16, z: i32) {
        self.write_long(((x as i64 & 0x3FFFFFFi64) << 38) | ((z as i64 & 0x3FFFFFF) << 12) | (y as i64 & 0xFFF))
    }

    pub fn write_var_int(&mut self, value: i32) {
        let mut current_value = value;

        loop {
            if (current_value & 0x7F.not()) == 0 {
                self.write_byte(current_value as u8);
                break;
            }

            self.write_byte(((current_value & 0x7F) | 0x80) as u8);

            current_value = (((current_value) as u32) >> 7) as i32;
        }
    }

    pub fn write_string(&mut self, str: &str) {
        self.write_var_int(str.len() as i32);
        self.write_all(str.as_bytes()).unwrap();
    }

    pub fn write_uuid(&mut self, uuid: Uuid) {
        let (msb, lsb) = uuid.as_u64_pair();
        self.write_long(msb as i64);
        self.write_long(lsb as i64);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }

}

impl Write for PacketWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl AsRef<[u8]> for PacketWriter {
    fn as_ref(&self) -> &[u8] {
        self.buf.as_ref()
    }
}

pub async fn write_var_int(target: &mut (impl AsyncWrite + Unpin), value: i32) -> std::io::Result<()> {
    let mut current_value = value;

    loop {
        if (current_value & 0x7F.not()) == 0 {
            target.write_all(&[current_value as u8]).await?;
            break;
        }

        target.write_all(&[((current_value & 0x7F) | 0x80) as u8]).await?;

        current_value = (((current_value) as u32) >> 7) as i32;
    }

    Ok(())
}