#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Algorithm {
    Sha256 = 0xb,
}

pub struct LoadedKeyName<'a> {
    algorithm: Algorithm,
    hash: &'a [u8],
}

impl<'a> LoadedKeyName<'a> {
    pub fn decode(data: &'a [u8]) -> Option<Self> {
        let algorithm = u16::from_be_bytes(data.get(..2)?.try_into().unwrap());
        match algorithm {
            0xb => {
                let hash = data.get(2..)?;
                if hash.len() != 32 {
                    error!("Invalid LKN: algorithm={} len={}", algorithm, data.len());
                    return None;
                }

                Some(Self {
                    algorithm: Algorithm::Sha256,
                    hash,
                })
            }
            _ => {
                error!("Unsupported algorithm ID=0x{:02x}", algorithm);
                None
            }
        }
    }

    #[inline]
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    #[inline]
    pub fn hash(&self) -> &[u8] {
        self.hash
    }
}
