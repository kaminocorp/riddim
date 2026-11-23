//! MIDI input.
//!
//! Incoming messages are parsed on the system's MIDI thread and pushed, timestamped, onto a
//! wait-free queue that the audio thread drains. System-exclusive and system real-time messages
//! are dropped here; the engine only consumes channel-voice messages.

use riddim_rt::spsc::Producer;

/// One channel-voice MIDI message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidiMessage {
    /// Host clock time of arrival in nanoseconds (0 if unknown).
    pub time_ns: u64,
    pub bytes: [u8; 3],
    /// 2 or 3.
    pub len: u8,
}

/// Splits a raw MIDI byte stream into channel-voice messages.
///
/// Handles running status, skips system-exclusive blocks, and ignores real-time bytes wherever
/// they appear (they are allowed in the middle of other messages). `running_status` carries
/// state between calls for transports that split messages across packets.
pub fn parse_stream(
    bytes: &[u8],
    time_ns: u64,
    running_status: &mut u8,
    mut emit: impl FnMut(MidiMessage),
) {
    let mut data = [0u8; 2];
    let mut have = 0usize;
    let mut in_sysex = false;
    for &byte in bytes {
        if byte >= 0xF8 {
            continue; // real-time: may interleave anywhere, never affects running status
        }
        if byte & 0x80 != 0 {
            in_sysex = byte == 0xF0;
            // System common messages cancel running status; channel messages set it.
            *running_status = if byte < 0xF0 { byte } else { 0 };
            have = 0;
            continue;
        }
        if in_sysex || *running_status == 0 {
            continue;
        }
        data[have] = byte;
        have += 1;
        let needed = match *running_status & 0xF0 {
            0xC0 | 0xD0 => 1,
            _ => 2,
        };
        if have == needed {
            emit(MidiMessage {
                time_ns,
                bytes: [
                    *running_status,
                    data[0],
                    if needed == 2 { data[1] } else { 0 },
                ],
                len: needed as u8 + 1,
            });
            have = 0;
        }
    }
}

/// A source of MIDI input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiPortInfo {
    pub name: String,
}

/// An open connection to every MIDI source on the system. Dropping it disconnects.
pub struct MidiInput {
    #[cfg(target_os = "macos")]
    _inner: macos::Connection,
    ports: Vec<MidiPortInfo>,
}

impl MidiInput {
    /// Connects to all current MIDI sources and forwards their messages to `sink`.
    ///
    /// On platforms without a MIDI backend this succeeds with no ports, so callers need no
    /// special case.
    pub fn open_all(client_name: &str, sink: Producer<MidiMessage>) -> Result<MidiInput, String> {
        #[cfg(target_os = "macos")]
        {
            let (inner, ports) = macos::Connection::open(client_name, sink)?;
            Ok(MidiInput {
                _inner: inner,
                ports,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (client_name, sink);
            Ok(MidiInput { ports: Vec::new() })
        }
    }

    pub fn ports(&self) -> &[MidiPortInfo] {
        &self.ports
    }
}

#[cfg(target_os = "macos")]
mod macos {
    //! CoreMIDI input. **Unverified on macOS.**

    use super::*;

    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }

    /// Converts mach host-time ticks to nanoseconds.
    fn ticks_to_ns(ticks: u64) -> u64 {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: valid out-pointer; the call cannot fail on supported systems.
        unsafe { mach_timebase_info(&mut info) };
        if info.denom == 0 {
            return ticks;
        }
        (ticks as u128 * info.numer as u128 / info.denom as u128) as u64
    }

    pub struct Connection {
        _client: coremidi::Client,
        _port: coremidi::InputPort,
    }

    impl Connection {
        pub fn open(
            client_name: &str,
            mut sink: Producer<MidiMessage>,
        ) -> Result<(Connection, Vec<MidiPortInfo>), String> {
            let client = coremidi::Client::new(client_name)
                .map_err(|status| format!("could not create a CoreMIDI client ({status})"))?;
            let mut running_status = 0u8;
            let port = client
                .input_port("riddim-in", move |packets: &coremidi::PacketList| {
                    for packet in packets.iter() {
                        let time_ns = ticks_to_ns(packet.timestamp());
                        parse_stream(packet.data(), time_ns, &mut running_status, |message| {
                            // A full queue means the audio thread is not draining; dropping is
                            // the only thing a MIDI thread can safely do.
                            let _ = sink.push(message);
                        });
                    }
                })
                .map_err(|status| format!("could not create a CoreMIDI input port ({status})"))?;

            let mut ports = Vec::new();
            for source in coremidi::Sources {
                if port.connect_source(&source).is_ok() {
                    ports.push(MidiPortInfo {
                        name: source
                            .display_name()
                            .unwrap_or_else(|| "MIDI source".to_string()),
                    });
                }
            }
            Ok((
                Connection {
                    _client: client,
                    _port: port,
                },
                ports,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<([u8; 3], u8)> {
        let mut out = Vec::new();
        let mut running = 0;
        parse_stream(bytes, 0, &mut running, |m| out.push((m.bytes, m.len)));
        out
    }

    #[test]
    fn plain_messages() {
        assert_eq!(
            parse(&[0x90, 60, 100, 0x80, 60, 0, 0xC1, 5, 0xE0, 1, 2]),
            vec![
                ([0x90, 60, 100], 3),
                ([0x80, 60, 0], 3),
                ([0xC1, 5, 0], 2),
                ([0xE0, 1, 2], 3)
            ]
        );
    }

    #[test]
    fn running_status_and_interleaved_realtime() {
        assert_eq!(
            parse(&[0x90, 60, 100, 62, 0xF8, 90, 64, 80]),
            vec![
                ([0x90, 60, 100], 3),
                ([0x90, 62, 90], 3),
                ([0x90, 64, 80], 3)
            ]
        );
    }

    #[test]
    fn sysex_is_skipped_and_cancels_running_status() {
        assert_eq!(
            parse(&[0x90, 60, 100, 0xF0, 1, 2, 3, 0xF7, 61, 100]),
            vec![([0x90, 60, 100], 3)]
        );
        assert_eq!(
            parse(&[0xF0, 0x7E, 0xF7, 0xB0, 7, 127]),
            vec![([0xB0, 7, 127], 3)]
        );
    }

    #[test]
    fn running_status_carries_across_packets() {
        let mut running = 0;
        let mut out = Vec::new();
        parse_stream(&[0x91, 40, 10], 1, &mut running, |m| out.push(m));
        parse_stream(&[41, 11], 2, &mut running, |m| out.push(m));
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[1],
            MidiMessage {
                time_ns: 2,
                bytes: [0x91, 41, 11],
                len: 3
            }
        );
    }

    #[test]
    fn truncated_and_stray_data_bytes_are_ignored() {
        assert_eq!(parse(&[1, 2, 3]), vec![]);
        assert_eq!(parse(&[0x90, 60]), vec![]);
    }
}
