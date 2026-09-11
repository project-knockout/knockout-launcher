//! Shared transport messages; no server or game-process implementation.
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub const PATH: &str = env!("KNOCKOUT_TRANSPORT_PATH");
pub type Id = [u8; 16];

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Register { proxy_port: u16, host_port: u16 },
    Ready { id: Id, proxy_port: u16 },
    UdpReady { challenge: Id },
    Ping,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        id: Id,
        key: [u8; 32],
        resume: String,
        udp: Option<String>,
    },
    Pair {
        id: Id,
        key: [u8; 32],
        host: bool,
        host_port: u16,
        allocation: String,
        candidate: Option<SocketAddr>,
    },
    Candidate {
        id: Id,
        address: SocketAddr,
    },
    Close {
        id: Id,
    },
    Pong,
}
