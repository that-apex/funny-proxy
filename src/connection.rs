use std::error::Error;
use std::fmt::format;
use std::io;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use base64::Engine;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use uuid::Uuid;

use crate::connection::ConnectionState::Disconnected;
use crate::packet::{DecodingError, Packet, PacketReader, PacketType, PacketWriter, write_var_int};

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub enum ConnectionState {
    Handshake,
    Status,
    Login,
    Play,
    Disconnected,
}

pub struct Connection {
    id: u64,
    stream: TcpStream,
    temp_buffer: Vec<u8>,
    current_packet: Vec<u8>,
    state: ConnectionState,
}

#[derive(Debug)]
enum ConnectionError {
    EndOfStream,
    Other(Box<dyn Error + Send + Sync>),
}

impl Connection {
    pub async fn process(&mut self) {
        self.log("connected");

        loop {
            match self.try_read().await {
                Ok(()) => {}
                Err(e) => {
                    let reason = format!("connection error: {:?}", e).to_string();
                    self.disconnect(&reason).await;
                    break;
                }
            }
        }

        self.log("disconnected");
    }

    async fn try_read(&mut self) -> Result<(), ConnectionError> {
        match self.stream.readable().await {
            Err(e) => return Err(ConnectionError::Other(e.into())),
            Ok(..) => {}
        }

        match self.stream.try_read_buf(&mut self.temp_buffer) {
            Ok(0) => {
                Err(ConnectionError::EndOfStream)
            }
            Ok(_n) => {
                self.data_read().await
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                Ok(())
            }
            Err(e) => {
                return Err(ConnectionError::Other(e.into()));
            }
        }
    }

    async fn data_read(&mut self) -> Result<(), ConnectionError> {
        if self.temp_buffer.is_empty() {
            return Ok(());
        }

        self.current_packet.append(&mut self.temp_buffer);
        self.temp_buffer.clear();

        loop {
            if self.state == Disconnected {
                return Ok(());
            }

            match self.try_to_parse_packet().await {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(());
                }
                Err(e) => return Err(e)
            }
        }
    }

    async fn try_to_parse_packet(&mut self) -> Result<bool, ConnectionError> {
        match Packet::decode(&self.current_packet, self.state).await {
            Ok(packet) => {
                self.current_packet.drain(0..packet.raw_size);
                self.handle_packet(packet).await?;

                Ok(true)
            }
            Err(DecodingError::PacketTooSmall) => Ok(false),
            Err(e) => Err(ConnectionError::Other(e.into()))
        }
    }

    async fn handle_packet(&mut self, mut packet: Packet) -> Result<(), ConnectionError> {
        self.log(format!("received packet of type: {:?} and length {}", packet.packet_type, packet.data.len()));

        let mut reader = PacketReader::create(&mut packet.data);

        match packet.packet_type {
            PacketType::HandshakeServerboundStart => {
                let protocol_version = reader.read_varint().unwrap();
                let host = reader.read_string(255).unwrap();
                let port = reader.read_short().unwrap();
                let next_state = reader.read_varint().unwrap();

                self.log(format!(
                    "client connected with protocol = {}, hostname = {}:{}, next_state = {}",
                    protocol_version, host, port, next_state
                ));

                match next_state {
                    1 => self.state = ConnectionState::Status,
                    2 => self.state = ConnectionState::Login,
                    _ => self.disconnect("state not supported").await
                }
            }
            PacketType::StatusServerboundRequest => {
                let mut packet = PacketWriter::create(1024);
                packet.write_packet_type(PacketType::StatusClientboundResponse);
                packet.write_string(r#"{
    "version": {
        "name": "1.19.4",
        "protocol": 762
    },
    "players": {
        "max": 100,
        "online": 5,
        "sample": []
    },
    "description": {
        "text": "Hello world"
    }
}"#);

                self.send_packet(&packet).await;
            }
            PacketType::StatusServerboundPing => {
                let value = reader.read_long().unwrap();

                let mut packet = PacketWriter::create(1024);
                packet.write_packet_type(PacketType::StatusClientboundPong);
                packet.write_long(value);
                self.send_packet(&packet).await;
            }
            PacketType::LoginServerboundStart => {
                let name = reader.read_string(16).unwrap();
                let uuid = reader.read_optional(|reader| reader.read_uuid()).unwrap();

                self.log(format!("Player logging in with name {} and uuid {:?}", name, uuid));

                let mut packet = PacketWriter::create(32);
                packet.write_packet_type(PacketType::LoginClientboundSuccess);
                packet.write_uuid(match uuid {
                    Some(id) => id,
                    None => Uuid::new_v4()
                });
                packet.write_string(&name);
                packet.write_var_int(0);

                self.send_packet(&packet).await;
                self.state = ConnectionState::Play;

                // TODO: Dump actual NBT for 1.19.4
                let nbt = base64::engine::general_purpose::STANDARD.decode("CgAACgATbWluZWNyYWZ0OmNoYXRfdHlwZQAKABhtaW5lY3JhZnQ6ZGltZW5zaW9uX3R5cGUACgAYbWluZWNyYWZ0OndvcmxkZ2VuL2Jpb21lAAA=").unwrap();

                packet.reset();
                packet.write_packet_type(PacketType::PlayClientboundLogin);
                packet.write_int(12); // entity id
                packet.write_boolean(false); // hardcore
                packet.write_byte(0); // gamemode
                packet.write_byte(0); // prev gamemode
                packet.write_var_int(1); // dimension count
                packet.write_string("minecraft:world"); // dimension id
                packet.write(nbt.as_slice()).expect("failed to write nbt");

                packet.write_string("minecraft:world"); // spawn dimension id
                packet.write_string("minecraft:world"); // spawn dimension name

                packet.write_long(0x7D42D4473EB771F9i64); // seed hash
                packet.write_var_int(0); // max players  (ignored)
                packet.write_var_int(10); // view distance
                packet.write_var_int(10); // simulation distance
                packet.write_boolean(false); // reduced debug info
                packet.write_boolean(true); // enable respawn screen
                packet.write_boolean(false); // is debug
                packet.write_boolean(false); // is flat
                packet.write_boolean(false); // has death location

                self.send_packet(&packet).await;

                packet.reset();
                packet.write_packet_type(PacketType::PlayClientboundDifficulty);
                packet.write_byte(2); // difficulty
                packet.write_boolean(false); // difficulty locked

                self.send_packet(&packet).await;

                packet.reset();
                packet.write_packet_type(PacketType::PlayClientboundAbilities);
                packet.write_byte(0); // difficulty
                packet.write_float(0.05); // fly speed
                packet.write_float(0.1); // fov modifier

                self.send_packet(&packet).await;

                packet.reset();
                packet.write_packet_type(PacketType::PlayClientboundSetDefaultSpawnPosition);
                packet.write_position(0, 100, 0); // position
                packet.write_float(0f32); // angle

                self.send_packet(&packet).await;

            }
            _ => self.disconnect("Invalid packet").await
        }


        Ok(())
    }

    async fn send_packet(&mut self, packet: &PacketWriter) {
        write_var_int(&mut self.stream, packet.len() as i32).await.expect("failed to write packet length");
        self.stream.write(packet.as_ref()).await.expect("failed to write a packet");
    }

    fn log<S: AsRef<str>>(&self, str: S) {
        println!("connection {}: {}", self.id, str.as_ref());
    }

    pub async fn disconnect(&mut self, reason: &str) {
        if self.state == Disconnected {
            return;
        }

        self.log(format!("disconnecting: {}", reason));
        self.state = Disconnected;
        self.stream.shutdown().await.expect("failed to shutdown");
    }

    pub fn create(stream: TcpStream) -> Connection {
        Connection {
            id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::SeqCst),
            stream,
            temp_buffer: Vec::with_capacity(4096),
            current_packet: Vec::with_capacity(4096),
            state: ConnectionState::Handshake,
        }
    }
}
