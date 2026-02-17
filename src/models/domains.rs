use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Wrapper for Ethereum addresses with hex string serialization
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainEthAddress(pub [u8; 20]);

impl DomainEthAddress {
    pub fn inner(&self) -> [u8; 20] {
        self.0
    }

    pub fn from_hex(s: &str) -> Result<Self, String> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|e| e.to_string())?;
        if bytes.len() != 20 {
            return Err(format!("Invalid address length: expected 20, got {}", bytes.len()));
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(DomainEthAddress(arr))
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl Serialize for DomainEthAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for DomainEthAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        DomainEthAddress::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for DomainEthAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// Wrapper for 32-byte values with hex string serialization
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainBytes32(pub [u8; 32]);

impl DomainBytes32 {
    pub fn inner(&self) -> [u8; 32] {
        self.0
    }

    pub fn from_hex(s: &str) -> Result<Self, String> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|e| e.to_string())?;
        if bytes.len() != 32 {
            return Err(format!("Invalid bytes32 length: expected 32, got {}", bytes.len()));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(DomainBytes32(arr))
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl Serialize for DomainBytes32 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for DomainBytes32 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        DomainBytes32::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for DomainBytes32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// Wrapper for U256 values with decimal string serialization
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainUint256(pub u128);

impl DomainUint256 {
    pub fn inner(&self) -> u128 {
        self.0
    }

    pub fn from_str(s: &str) -> Result<Self, String> {
        let val: u128 = s.parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        Ok(DomainUint256(val))
    }
}

impl From<u64> for DomainUint256 {
    fn from(val: u64) -> Self {
        DomainUint256(val as u128)
    }
}

impl From<u128> for DomainUint256 {
    fn from(val: u128) -> Self {
        DomainUint256(val)
    }
}

impl Serialize for DomainUint256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for DomainUint256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        DomainUint256::from_str(&s).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for DomainUint256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
