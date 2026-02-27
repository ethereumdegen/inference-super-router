pub mod sign;
pub mod types;
pub mod verify;

pub use sign::Erc8128Signer;
#[allow(unused_imports)]
pub use types::{Erc8128Error, Erc8128Identity, Erc8128SignedHeaders};
pub use verify::{has_erc8128_headers, verify_from_request};

use sha3::{Digest, Keccak256};

/// EIP-55 mixed-case checksum encoding for an Ethereum address.
pub fn to_checksum_address(addr_bytes: &[u8; 20]) -> String {
    let hex_addr = hex::encode(addr_bytes);
    let mut hasher = Keccak256::new();
    hasher.update(hex_addr.as_bytes());
    let hash = hasher.finalize();

    let mut checksummed = String::with_capacity(42);
    checksummed.push_str("0x");
    for (i, c) in hex_addr.chars().enumerate() {
        if c.is_ascii_alphabetic() {
            let hash_byte = hash[i / 2];
            let nibble = if i % 2 == 0 { hash_byte >> 4 } else { hash_byte & 0x0f };
            if nibble >= 8 {
                checksummed.push(c.to_ascii_uppercase());
            } else {
                checksummed.push(c);
            }
        } else {
            checksummed.push(c);
        }
    }
    checksummed
}
