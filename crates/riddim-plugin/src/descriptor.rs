//! What a plugin is, without loading it.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Plugin binary format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginFormat {
    Vst3,
    #[serde(rename = "au")]
    AudioUnit,
    Clap,
    /// Built into Riddim (test and utility processors).
    Internal,
}

impl PluginFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            PluginFormat::Vst3 => "vst3",
            PluginFormat::AudioUnit => "au",
            PluginFormat::Clap => "clap",
            PluginFormat::Internal => "internal",
        }
    }
}

impl fmt::Display for PluginFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PluginFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "vst3" => Ok(PluginFormat::Vst3),
            "au" | "audiounit" => Ok(PluginFormat::AudioUnit),
            "clap" => Ok(PluginFormat::Clap),
            "internal" => Ok(PluginFormat::Internal),
            other => Err(format!("unknown plugin format '{other}'")),
        }
    }
}

/// Stable identity of a plugin: its format plus the format's own unique id.
///
/// * VST3: the processor class id as 32 upper-case hex digits.
/// * Audio Unit: `type:subtype:manufacturer` four-character codes, e.g. `aumu:NiK8:-NI-`.
/// * CLAP: the plugin id string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PluginId {
    pub format: PluginFormat,
    pub uid: String,
}

impl PluginId {
    pub fn new(format: PluginFormat, uid: impl Into<String>) -> Self {
        Self {
            format,
            uid: uid.into(),
        }
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.format, self.uid)
    }
}

impl FromStr for PluginId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (format, uid) = s
            .split_once(':')
            .ok_or_else(|| format!("expected '<format>:<id>', got '{s}'"))?;
        if uid.is_empty() {
            return Err(format!("empty plugin id in '{s}'"));
        }
        Ok(PluginId {
            format: format.parse()?,
            uid: uid.to_string(),
        })
    }
}

/// Broad role of a plugin, used for browsing and default routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Instrument,
    Effect,
    MidiEffect,
    Generator,
    Other,
}

/// Everything known about a plugin from scanning it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDescriptor {
    pub id: PluginId,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub kind: PluginKind,
    /// The format's own category strings, unparsed (VST3 sub-categories, AU type code, ...).
    #[serde(default)]
    pub categories: Vec<String>,
    /// Bundle or file the plugin was found in. `None` for formats with a system registry (AU).
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BusDirection {
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BusKind {
    Main,
    /// Sidechain inputs and auxiliary outputs.
    Aux,
}

/// One audio bus of a plugin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusInfo {
    pub name: String,
    pub channels: u32,
    pub kind: BusKind,
    /// Whether the bus takes part in processing. Inactive buses get no buffers.
    pub active: bool,
}

/// The buses of a plugin instance, in the plugin's own order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusLayout {
    pub audio_inputs: Vec<BusInfo>,
    pub audio_outputs: Vec<BusInfo>,
    /// Number of event (note/MIDI) input buses.
    pub event_inputs: u32,
    /// Number of event (note/MIDI) output buses.
    pub event_outputs: u32,
}

impl BusLayout {
    /// Total channels across active input buses: the length of `ProcessContext::inputs`.
    pub fn active_input_channels(&self) -> usize {
        self.audio_inputs
            .iter()
            .filter(|b| b.active)
            .map(|b| b.channels as usize)
            .sum()
    }

    /// Total channels across active output buses: the length of `ProcessContext::outputs`.
    pub fn active_output_channels(&self) -> usize {
        self.audio_outputs
            .iter()
            .filter(|b| b.active)
            .map(|b| b.channels as usize)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_id_round_trips_through_text() {
        let id = PluginId::new(PluginFormat::AudioUnit, "aumu:NiK8:-NI-");
        let text = id.to_string();
        assert_eq!(text, "au:aumu:NiK8:-NI-");
        assert_eq!(text.parse::<PluginId>().unwrap(), id);
        assert!("nonsense".parse::<PluginId>().is_err());
        assert!("vst3:".parse::<PluginId>().is_err());
    }

    #[test]
    fn descriptor_round_trips_through_json() {
        let d = PluginDescriptor {
            id: PluginId::new(PluginFormat::Vst3, "00112233445566778899AABBCCDDEEFF"),
            name: "Test".into(),
            vendor: "Riddim".into(),
            version: "1.0.0".into(),
            kind: PluginKind::Instrument,
            categories: vec!["Instrument".into(), "Synth".into()],
            path: Some("/tmp/Test.vst3".into()),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<PluginDescriptor>(&json).unwrap(), d);
    }
}
