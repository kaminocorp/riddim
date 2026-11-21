//! Opaque, versioned plugin state.

use serde::{Deserialize, Serialize};

use crate::descriptor::{PluginFormat, PluginId};
use crate::error::{PluginError, Result};

/// A plugin's saved state, tagged with the identity of the plugin that produced it.
///
/// The payload is whatever the format defines (VST3 component and controller chunks, the AU
/// `ClassInfo` property list, a CLAP state stream). Riddim never looks inside. A project keeps
/// the blob even when the plugin is missing, so it survives a round trip on another machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateBlob {
    pub plugin: PluginId,
    pub data: Vec<u8>,
}

const MAGIC: &[u8; 4] = b"RDPS";
const VERSION: u32 = 1;

impl StateBlob {
    pub fn new(plugin: PluginId, data: Vec<u8>) -> Self {
        Self { plugin, data }
    }

    /// Fails unless this blob was produced by `plugin`.
    pub fn expect_plugin(&self, plugin: &PluginId) -> Result<()> {
        if &self.plugin == plugin {
            Ok(())
        } else {
            Err(PluginError::State(format!(
                "state belongs to {} but was given to {}",
                self.plugin, plugin
            )))
        }
    }

    /// Serialises to a self-describing binary container (for `.rstate` files).
    pub fn to_bytes(&self) -> Vec<u8> {
        let uid = self.plugin.uid.as_bytes();
        let mut out = Vec::with_capacity(4 + 4 + 1 + 4 + uid.len() + 8 + self.data.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.push(format_code(self.plugin.format));
        out.extend_from_slice(&(uid.len() as u32).to_le_bytes());
        out.extend_from_slice(uid);
        out.extend_from_slice(&(self.data.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// Parses the container written by [`to_bytes`](Self::to_bytes).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader { bytes, pos: 0 };
        if r.take(4)? != MAGIC {
            return Err(PluginError::State("not a Riddim plugin state file".into()));
        }
        let version = r.u32()?;
        if version != VERSION {
            return Err(PluginError::State(format!("unsupported state version {version}")));
        }
        let format = format_from_code(r.take(1)?[0])?;
        let uid_len = r.u32()? as usize;
        let uid = std::str::from_utf8(r.take(uid_len)?)
            .map_err(|_| PluginError::State("plugin id is not UTF-8".into()))?
            .to_string();
        let data_len = r.u64()? as usize;
        let data = r.take(data_len)?.to_vec();
        if r.pos != bytes.len() {
            return Err(PluginError::State("trailing bytes after state".into()));
        }
        Ok(StateBlob { plugin: PluginId { format, uid }, data })
    }
}

fn format_code(format: PluginFormat) -> u8 {
    match format {
        PluginFormat::Vst3 => 1,
        PluginFormat::AudioUnit => 2,
        PluginFormat::Clap => 3,
        PluginFormat::Internal => 4,
    }
}

fn format_from_code(code: u8) -> Result<PluginFormat> {
    Ok(match code {
        1 => PluginFormat::Vst3,
        2 => PluginFormat::AudioUnit,
        3 => PluginFormat::Clap,
        4 => PluginFormat::Internal,
        other => return Err(PluginError::State(format!("unknown format code {other}"))),
    })
}

/// A bounds-checked little-endian reader shared by the state containers.
pub(crate) struct Reader<'a> {
    pub bytes: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| PluginError::State("state is truncated".into()))?;
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_round_trips() {
        let blob = StateBlob::new(PluginId::new(PluginFormat::Vst3, "ABCDEF"), vec![1, 2, 3, 255]);
        let bytes = blob.to_bytes();
        assert_eq!(StateBlob::from_bytes(&bytes).unwrap(), blob);
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        let blob = StateBlob::new(PluginId::new(PluginFormat::AudioUnit, "aumu:x:y"), vec![9; 40]);
        let bytes = blob.to_bytes();
        assert!(StateBlob::from_bytes(&bytes[..bytes.len() - 1]).is_err());
        assert!(StateBlob::from_bytes(b"nope").is_err());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(StateBlob::from_bytes(&extra).is_err());
    }

    #[test]
    fn expect_plugin_checks_identity() {
        let id = PluginId::new(PluginFormat::Vst3, "A");
        let blob = StateBlob::new(id.clone(), vec![]);
        assert!(blob.expect_plugin(&id).is_ok());
        assert!(blob.expect_plugin(&PluginId::new(PluginFormat::Vst3, "B")).is_err());
    }
}
