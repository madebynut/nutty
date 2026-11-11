//! Per-connection SSH algorithm preferences resolved against the backend's supported algorithms.

use std::{borrow::Cow, collections::HashSet};

use anyhow::{Result, ensure};
use russh::{Preferred, cipher, compression, kex, keys::Algorithm, mac};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Kex,
    Key,
    Cipher,
    Mac,
    Compression,
}
impl Category {
    pub const ALL: [Self; 5] = [
        Self::Kex,
        Self::Key,
        Self::Cipher,
        Self::Mac,
        Self::Compression,
    ];
    pub fn label(self) -> &'static str {
        crate::i18n::text(match self {
            Self::Kex => "密钥交换（KEX）",
            Self::Key => "主机密钥",
            Self::Cipher => "加密算法",
            Self::Mac => "完整性校验（MAC）",
            Self::Compression => "压缩",
        })
    }
    pub fn supported(self) -> Vec<String> {
        match self {
            Self::Kex => kex::ALL_KEX_ALGORITHMS
                .iter()
                .filter(|n| ***n != kex::NONE)
                .map(|n| n.as_ref().to_string())
                .collect(),
            Self::Key => [
                "ssh-ed25519",
                "ecdsa-sha2-nistp256",
                "ecdsa-sha2-nistp384",
                "ecdsa-sha2-nistp521",
                "rsa-sha2-512",
                "rsa-sha2-256",
                "ssh-rsa",
            ]
            .map(String::from)
            .to_vec(),
            Self::Cipher => cipher::ALL_CIPHERS
                .iter()
                .filter(|n| ***n != cipher::NONE && ***n != cipher::CLEAR)
                .map(|n| n.as_ref().to_string())
                .collect(),
            Self::Mac => mac::ALL_MAC_ALGORITHMS
                .iter()
                .filter(|n| ***n != mac::NONE)
                .map(|n| n.as_ref().to_string())
                .collect(),
            Self::Compression => compression::ALL_COMPRESSION_ALGORITHMS
                .iter()
                .map(|n| n.as_ref().to_string())
                .collect(),
        }
    }
    pub fn defaults(self) -> Vec<String> {
        let preferred = Preferred::default();
        match self {
            Self::Kex => preferred
                .kex
                .iter()
                .filter(|n| self.supported().iter().any(|s| s == n.as_ref()))
                .map(|n| n.as_ref().to_string())
                .collect(),
            Self::Key => preferred.key.iter().map(ToString::to_string).collect(),
            Self::Cipher => preferred
                .cipher
                .iter()
                .map(|n| n.as_ref().to_string())
                .collect(),
            Self::Mac => preferred
                .mac
                .iter()
                .map(|n| n.as_ref().to_string())
                .collect(),
            Self::Compression => preferred
                .compression
                .iter()
                .map(|n| n.as_ref().to_string())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Algorithms {
    kex: Option<Vec<String>>,
    key: Option<Vec<String>>,
    cipher: Option<Vec<String>>,
    mac: Option<Vec<String>>,
    compression: Option<Vec<String>>,
}
impl Algorithms {
    pub fn values(&self, category: Category) -> &Option<Vec<String>> {
        match category {
            Category::Kex => &self.kex,
            Category::Key => &self.key,
            Category::Cipher => &self.cipher,
            Category::Mac => &self.mac,
            Category::Compression => &self.compression,
        }
    }
    pub fn values_mut(&mut self, category: Category) -> &mut Option<Vec<String>> {
        match category {
            Category::Kex => &mut self.kex,
            Category::Key => &mut self.key,
            Category::Cipher => &mut self.cipher,
            Category::Mac => &mut self.mac,
            Category::Compression => &mut self.compression,
        }
    }
    pub fn selected(&self, category: Category) -> Vec<String> {
        self.values(category)
            .clone()
            .unwrap_or_else(|| category.defaults())
    }
    pub fn validate(&self) -> Result<()> {
        for category in Category::ALL {
            if let Some(values) = self.values(category) {
                let supported = category.supported();
                let unique: HashSet<_> = values.iter().collect();
                ensure!(
                    !values.is_empty()
                        && values.len() <= 32
                        && unique.len() == values.len()
                        && values.iter().all(|value| supported.contains(value)),
                    "SSH 算法配置无效"
                );
            }
        }
        Ok(())
    }
    pub fn preferred(&self) -> Result<Preferred> {
        self.validate()?;
        let mut result = Preferred::default();
        if let Some(values) = &self.kex {
            let mut values: Vec<_> = values
                .iter()
                .map(|v| {
                    kex::Name::try_from(v.as_str()).map_err(|_| anyhow::anyhow!("SSH KEX 不可用"))
                })
                .collect::<Result<_>>()?;
            // Negotiation markers are capabilities, not selectable algorithms.
            values.extend([
                kex::EXTENSION_SUPPORT_AS_CLIENT,
                kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
            ]);
            result.kex = Cow::Owned(values);
        }
        if let Some(values) = &self.key {
            result.key = Cow::Owned(
                values
                    .iter()
                    .map(|v| Algorithm::new(v).map_err(anyhow::Error::from))
                    .collect::<Result<_>>()?,
            );
        }
        if let Some(values) = &self.cipher {
            result.cipher = Cow::Owned(
                values
                    .iter()
                    .map(|v| {
                        cipher::Name::try_from(v.as_str())
                            .map_err(|_| anyhow::anyhow!("SSH 加密算法不可用"))
                    })
                    .collect::<Result<_>>()?,
            );
        }
        if let Some(values) = &self.mac {
            result.mac = Cow::Owned(
                values
                    .iter()
                    .map(|v| {
                        mac::Name::try_from(v.as_str())
                            .map_err(|_| anyhow::anyhow!("SSH MAC 不可用"))
                    })
                    .collect::<Result<_>>()?,
            );
        }
        if let Some(values) = &self.compression {
            result.compression = Cow::Owned(
                values
                    .iter()
                    .map(|v| {
                        compression::Name::try_from(v.as_str())
                            .map_err(|_| anyhow::anyhow!("SSH 压缩算法不可用"))
                    })
                    .collect::<Result<_>>()?,
            );
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_preferences_keep_order_and_negotiation_extensions() -> Result<()> {
        let mut algorithms = Algorithms::default();
        *algorithms.values_mut(Category::Kex) = Some(vec!["curve25519-sha256".into()]);
        *algorithms.values_mut(Category::Cipher) =
            Some(vec!["aes256-ctr".into(), "aes128-ctr".into()]);
        let p = algorithms.preferred()?;
        assert_eq!(p.kex[0], kex::CURVE25519);
        assert!(p.kex.contains(&kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT));
        assert_eq!(&*p.cipher, &[cipher::AES_256_CTR, cipher::AES_128_CTR]);
        *algorithms.values_mut(Category::Cipher) = Some(vec!["none".into()]);
        assert!(algorithms.preferred().is_err());
        *algorithms.values_mut(Category::Cipher) = Some(Vec::new());
        assert!(algorithms.preferred().is_err());
        Ok(())
    }
}
