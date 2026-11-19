use anyhow::{Result, anyhow, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::TryRng as _;
use rand::rngs::SysRng;

const DOMAIN_H2C: &str = "usher-v1-host-to-client";
const DOMAIN_C2H: &str = "usher-v1-client-to-host";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Client,
}

/// Generates 32 cryptographically random bytes from the OS CSPRNG.
pub fn generate_session_key() -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut key)
        .map_err(|e| anyhow!("OS CSPRNG failure: {e}"))?;
    Ok(key)
}

/// 128-packet sliding anti-replay window.
#[derive(Debug, Clone, Default)]
pub struct ReplayWindow {
    initialized: bool,
    highest_seq: u64,
    bitmap: u128,
}

impl ReplayWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cheap pre-AEAD check; does not mutate window state.
    pub fn is_acceptable(&self, seq: u64) -> bool {
        if !self.initialized || seq > self.highest_seq {
            return true;
        }
        let diff = self.highest_seq - seq;
        if diff >= 128 {
            return false;
        }
        (self.bitmap & (1u128 << diff)) == 0
    }

    /// Records `seq` after AEAD verification succeeds.
    pub fn record_accepted(&mut self, seq: u64) {
        if !self.initialized {
            self.initialized = true;
            self.highest_seq = seq;
            self.bitmap = 1;
            return;
        }
        if seq > self.highest_seq {
            let shift = seq - self.highest_seq;
            self.bitmap = if shift >= 128 {
                1
            } else {
                (self.bitmap << shift) | 1
            };
            self.highest_seq = seq;
        } else {
            let diff = self.highest_seq - seq;
            if diff < 128 {
                self.bitmap |= 1u128 << diff;
            }
        }
    }
}

pub struct SessionCrypto {
    tx_cipher: ChaCha20Poly1305,
    rx_cipher: ChaCha20Poly1305,
    tx_seq: u64,
    rx_window: ReplayWindow,
}

impl SessionCrypto {
    pub fn new(session_key: &[u8; 32], role: Role) -> Self {
        let h2c_bytes = blake3::derive_key(DOMAIN_H2C, session_key);
        let c2h_bytes = blake3::derive_key(DOMAIN_C2H, session_key);

        let (tx_bytes, rx_bytes) = match role {
            Role::Host => (h2c_bytes, c2h_bytes),
            Role::Client => (c2h_bytes, h2c_bytes),
        };

        let tx_cipher = ChaCha20Poly1305::new(&Key::from(tx_bytes));
        let rx_cipher = ChaCha20Poly1305::new(&Key::from(rx_bytes));

        Self {
            tx_cipher,
            rx_cipher,
            tx_seq: 0,
            rx_window: ReplayWindow::new(),
        }
    }

    fn nonce_for_seq(seq: u64) -> Nonce {
        let mut nonce = [0u8; 12];
        nonce[4..12].copy_from_slice(&seq.to_le_bytes());
        Nonce::from(nonce)
    }

    /// Allocates a strictly unique monotonic sequence number and encrypts `plaintext`
    /// with AAD constructed from that sequence number.
    pub fn encrypt_with_seq<F, const N: usize>(
        &mut self,
        build_aad: F,
        plaintext: &[u8],
    ) -> Result<([u8; N], Vec<u8>)>
    where
        F: FnOnce(u64) -> [u8; N],
    {
        if self.tx_seq == u64::MAX {
            bail!("sequence counter exhausted");
        }
        let seq = self.tx_seq;
        self.tx_seq += 1;

        let aad = build_aad(seq);
        let nonce = Self::nonce_for_seq(seq);
        let ciphertext = self
            .tx_cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow!("AEAD encryption failed"))?;

        Ok((aad, ciphertext))
    }

    /// Verifies anti-replay and decrypts `ciphertext` with `aad`.
    /// Replay state is only updated when authentication succeeds.
    pub fn decrypt(&mut self, seq: u64, aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
        if !self.rx_window.is_acceptable(seq) {
            bail!("replayed or stale sequence number {seq}");
        }
        let nonce = Self::nonce_for_seq(seq);
        let plaintext = self
            .rx_cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| anyhow!("AEAD authentication failed"))?;

        self.rx_window.record_accepted(seq);
        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_direction_isolation() {
        let key = generate_session_key().unwrap();
        let mut host = SessionCrypto::new(&key, Role::Host);
        let mut client = SessionCrypto::new(&key, Role::Client);

        let (aad, ct) = host
            .encrypt_with_seq(|seq| seq.to_le_bytes(), b"hello client")
            .unwrap();
        let seq = u64::from_le_bytes(aad);
        let pt = client.decrypt(seq, &aad, &ct).unwrap();
        assert_eq!(pt, b"hello client");

        // Host cannot decrypt its own H2C ciphertext as C2H
        assert!(host.decrypt(seq, &aad, &ct).is_err());

        // Replaying the same packet is rejected
        assert!(client.decrypt(seq, &aad, &ct).is_err());
    }

    #[test]
    fn tampered_ciphertext_does_not_advance_replay_window() {
        let key = [7u8; 32];
        let mut host = SessionCrypto::new(&key, Role::Host);
        let mut client = SessionCrypto::new(&key, Role::Client);

        let (aad, mut ct) = host
            .encrypt_with_seq(|seq| seq.to_le_bytes(), b"secret")
            .unwrap();
        let seq = u64::from_le_bytes(aad);
        ct[0] ^= 0xff;
        assert!(client.decrypt(seq, &aad, &ct).is_err());

        // Original authentic packet with the same seq still succeeds
        ct[0] ^= 0xff;
        assert_eq!(client.decrypt(seq, &aad, &ct).unwrap(), b"secret");
    }

    #[test]
    fn wrong_key_and_stale_seq_rejected() {
        let mut host = SessionCrypto::new(&[1u8; 32], Role::Host);
        let mut wrong_client = SessionCrypto::new(&[2u8; 32], Role::Client);
        let mut right_client = SessionCrypto::new(&[1u8; 32], Role::Client);

        let (aad0, ct0) = host
            .encrypt_with_seq(|seq| seq.to_le_bytes(), b"pkt0")
            .unwrap();
        assert!(wrong_client.decrypt(0, &aad0, &ct0).is_err());

        // Advance host + right_client by 150 packets
        for _ in 1..=150 {
            let (aad, ct) = host
                .encrypt_with_seq(|seq| seq.to_le_bytes(), b"pkt")
                .unwrap();
            let seq = u64::from_le_bytes(aad);
            let _ = right_client.decrypt(seq, &aad, &ct).unwrap();
        }

        // Now packet 0 is outside the 128-packet window
        assert!(right_client.decrypt(0, &aad0, &ct0).is_err());
    }

    #[test]
    fn out_of_order_within_window_accepted_once() {
        let key = [5u8; 32];
        let mut host = SessionCrypto::new(&key, Role::Host);
        let mut client = SessionCrypto::new(&key, Role::Client);

        let mut pkts = Vec::new();
        for i in 0u8..5 {
            let (aad, ct) = host.encrypt_with_seq(|seq| seq.to_le_bytes(), &[i]).unwrap();
            pkts.push((u64::from_le_bytes(aad), aad, ct));
        }

        // Receive 3, then 1 (out-of-order), then 1 again (replay), then 2, 0, 4.
        assert_eq!(client.decrypt(pkts[3].0, &pkts[3].1, &pkts[3].2).unwrap(), vec![3]);
        assert_eq!(client.decrypt(pkts[1].0, &pkts[1].1, &pkts[1].2).unwrap(), vec![1]);
        assert!(client.decrypt(pkts[1].0, &pkts[1].1, &pkts[1].2).is_err());
        assert_eq!(client.decrypt(pkts[2].0, &pkts[2].1, &pkts[2].2).unwrap(), vec![2]);
        assert_eq!(client.decrypt(pkts[0].0, &pkts[0].1, &pkts[0].2).unwrap(), vec![0]);
        assert_eq!(client.decrypt(pkts[4].0, &pkts[4].1, &pkts[4].2).unwrap(), vec![4]);
    }
}
