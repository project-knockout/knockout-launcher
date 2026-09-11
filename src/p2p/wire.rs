//! Authenticated datagrams shared by the direct and relayed player transports.
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use rand_core::{OsRng, RngCore};

pub const HEADER: usize = 25;
pub const MAX_GAME_PACKET: usize = 65_507;
pub const MAX_FRAME: usize = HEADER + 16 + 1 + MAX_GAME_PACKET;
pub const DATA: u8 = 0;
pub const PROBE: u8 = 1;
pub const ACK: u8 = 2;
pub const REGISTER: u8 = 3;

pub fn random<const N: usize>() -> [u8; N] {
    let mut value = [0; N];
    OsRng.fill_bytes(&mut value);
    value
}

/// A bounded packet window permits UDP reordering without accepting duplicates.
#[derive(Default)]
pub struct PacketWindow {
    highest: Option<u64>,
    seen: u128,
}

impl PacketWindow {
    fn accepts(&self, sequence: u64) -> bool {
        match self.highest {
            None => true,
            Some(highest) if sequence > highest => true,
            Some(highest) => {
                highest - sequence < 128 && self.seen & (1 << (highest - sequence)) == 0
            }
        }
    }
    fn commit(&mut self, sequence: u64) {
        match self.highest {
            None => {
                self.highest = Some(sequence);
                self.seen = 1;
            }
            Some(highest) if sequence > highest => {
                self.seen = if sequence - highest >= 128 {
                    1
                } else {
                    (self.seen << (sequence - highest)) | 1
                };
                self.highest = Some(sequence);
            }
            Some(highest) => self.seen |= 1 << (highest - sequence),
        }
    }
}

pub struct Cipher {
    id: [u8; 16],
    role: u8,
    key: ChaCha20Poly1305,
    sequence: u64,
    received: PacketWindow,
}

impl Cipher {
    pub fn new(id: [u8; 16], key: [u8; 32], role: u8) -> Self {
        Self {
            id,
            role,
            key: ChaCha20Poly1305::new((&key).into()),
            sequence: 0,
            received: PacketWindow::default(),
        }
    }
    pub fn seal(&mut self, kind: u8, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() > MAX_GAME_PACKET {
            return None;
        }
        let sequence = self.sequence;
        self.sequence = sequence.checked_add(1)?;
        let mut frame = self.id.to_vec();
        frame.push(self.role);
        frame.extend_from_slice(&sequence.to_be_bytes());
        let mut nonce = [0; 12];
        nonce[3] = self.role;
        nonce[4..].copy_from_slice(&sequence.to_be_bytes());
        let mut plain = Vec::with_capacity(data.len() + 1);
        plain.push(kind);
        plain.extend_from_slice(data);
        let encrypted = self
            .key
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plain,
                    aad: &frame,
                },
            )
            .ok()?;
        frame.extend_from_slice(&encrypted);
        Some(frame)
    }
    pub fn open(&mut self, frame: &[u8], expected_role: u8) -> Option<(u8, Vec<u8>)> {
        if frame.len() < HEADER + 17
            || frame.len() > MAX_FRAME
            || frame[..16] != self.id
            || frame[16] != expected_role
        {
            return None;
        }
        let sequence = u64::from_be_bytes(frame[17..HEADER].try_into().ok()?);
        if !self.received.accepts(sequence) {
            return None;
        }
        let mut nonce = [0; 12];
        nonce[3] = expected_role;
        nonce[4..].copy_from_slice(&sequence.to_be_bytes());
        let plain = self
            .key
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &frame[HEADER..],
                    aad: &frame[..HEADER],
                },
            )
            .ok()?;
        let (&kind, data) = plain.split_first()?;
        self.received.commit(sequence);
        Some((kind, data.to_vec()))
    }
}
