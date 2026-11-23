//! Sample-timestamped events passed to and from a plugin within one block.

/// An event at a sample offset inside the current block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    /// Offset in frames from the start of the block. Lists are sorted by this.
    pub offset: u32,
    pub body: EventBody,
}

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventBody {
    /// `velocity` is 0..=1. `note_id` is -1 when the sender does not track note identities.
    NoteOn {
        channel: u8,
        key: u8,
        velocity: f32,
        note_id: i32,
    },
    NoteOff {
        channel: u8,
        key: u8,
        velocity: f32,
        note_id: i32,
    },
    /// Any other channel-voice MIDI message (controllers, pitch bend, pressure, program change),
    /// as raw status and data bytes. `len` is 2 or 3.
    Midi { data: [u8; 3], len: u8 },
    /// A parameter value, normalised to 0..=1. Host to plugin: automation or a UI edit. Plugin to
    /// host: the plugin changed its own parameter while processing.
    Param { id: u32, value: f64 },
}

impl Event {
    pub fn note_on(offset: u32, channel: u8, key: u8, velocity: f32) -> Self {
        Event {
            offset,
            body: EventBody::NoteOn {
                channel,
                key,
                velocity,
                note_id: -1,
            },
        }
    }

    pub fn note_off(offset: u32, channel: u8, key: u8) -> Self {
        Event {
            offset,
            body: EventBody::NoteOff {
                channel,
                key,
                velocity: 0.0,
                note_id: -1,
            },
        }
    }

    pub fn param(offset: u32, id: u32, value: f64) -> Self {
        Event {
            offset,
            body: EventBody::Param { id, value },
        }
    }

    /// Builds an event from a raw channel-voice MIDI message. Note-on with velocity 0 becomes a
    /// note-off. Returns `None` for system messages and truncated data.
    pub fn from_midi(offset: u32, bytes: &[u8]) -> Option<Self> {
        let status = *bytes.first()?;
        if !(0x80..0xF0).contains(&status) {
            return None;
        }
        let channel = status & 0x0F;
        let kind = status & 0xF0;
        let needed = if kind == 0xC0 || kind == 0xD0 { 2 } else { 3 };
        if bytes.len() < needed {
            return None;
        }
        let d1 = bytes[1] & 0x7F;
        let d2 = if needed == 3 { bytes[2] & 0x7F } else { 0 };
        let body = match kind {
            0x90 if d2 > 0 => EventBody::NoteOn {
                channel,
                key: d1,
                velocity: d2 as f32 / 127.0,
                note_id: -1,
            },
            0x90 => EventBody::NoteOff {
                channel,
                key: d1,
                velocity: 0.0,
                note_id: -1,
            },
            0x80 => EventBody::NoteOff {
                channel,
                key: d1,
                velocity: d2 as f32 / 127.0,
                note_id: -1,
            },
            _ => EventBody::Midi {
                data: [status, d1, d2],
                len: needed as u8,
            },
        };
        Some(Event { offset, body })
    }

    /// The raw MIDI form of this event, if it has one.
    pub fn to_midi(&self) -> Option<([u8; 3], usize)> {
        let vel = |v: f32| (v.clamp(0.0, 1.0) * 127.0).round() as u8;
        match self.body {
            EventBody::NoteOn {
                channel,
                key,
                velocity,
                ..
            } => Some((
                [0x90 | (channel & 0x0F), key & 0x7F, vel(velocity).max(1)],
                3,
            )),
            EventBody::NoteOff {
                channel,
                key,
                velocity,
                ..
            } => Some(([0x80 | (channel & 0x0F), key & 0x7F, vel(velocity)], 3)),
            EventBody::Midi { data, len } => Some((data, len as usize)),
            EventBody::Param { .. } => None,
        }
    }
}

/// A fixed-capacity event list. Pushing never allocates; events beyond capacity are dropped and
/// counted.
#[derive(Debug)]
pub struct EventBuffer {
    events: Vec<Event>,
    dropped: u32,
}

impl EventBuffer {
    /// Allocates room for `capacity` events. Call off the audio thread.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
            dropped: 0,
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.events.clear();
        self.dropped = 0;
    }

    /// Appends an event; returns `false` (and counts a drop) if the buffer is full.
    #[inline]
    pub fn push(&mut self, event: Event) -> bool {
        if self.events.len() < self.events.capacity() {
            self.events.push(event);
            true
        } else {
            self.dropped = self.dropped.saturating_add(1);
            false
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[Event] {
        &self.events
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.events.capacity()
    }

    /// Events dropped since the last [`clear`](Self::clear) because the buffer was full.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Stable sort by offset. Insertion sort: the lists are short and nearly sorted, and it
    /// needs no scratch memory.
    pub fn sort(&mut self) {
        let ev = &mut self.events;
        for i in 1..ev.len() {
            let mut j = i;
            while j > 0 && ev[j - 1].offset > ev[j].offset {
                ev.swap(j - 1, j);
                j -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_round_trip() {
        let on = Event::from_midi(3, &[0x91, 60, 100]).unwrap();
        assert!(matches!(
            on.body,
            EventBody::NoteOn {
                channel: 1,
                key: 60,
                ..
            }
        ));
        assert_eq!(on.to_midi().unwrap(), ([0x91, 60, 100], 3));

        let off = Event::from_midi(0, &[0x90, 60, 0]).unwrap();
        assert!(matches!(
            off.body,
            EventBody::NoteOff {
                channel: 0,
                key: 60,
                ..
            }
        ));

        let cc = Event::from_midi(0, &[0xB0, 64, 127]).unwrap();
        assert_eq!(cc.to_midi().unwrap(), ([0xB0, 64, 127], 3));

        let pc = Event::from_midi(0, &[0xC2, 5]).unwrap();
        assert_eq!(pc.to_midi().unwrap(), ([0xC2, 5, 0], 2));

        assert!(Event::from_midi(0, &[0xF8]).is_none());
        assert!(Event::from_midi(0, &[0x90, 60]).is_none());
        assert!(Event::from_midi(0, &[]).is_none());
    }

    #[test]
    fn buffer_drops_when_full_and_sorts_stably() {
        let mut buf = EventBuffer::with_capacity(3);
        assert!(buf.push(Event::note_on(5, 0, 60, 1.0)));
        assert!(buf.push(Event::note_on(1, 0, 61, 1.0)));
        assert!(buf.push(Event::note_off(1, 0, 62)));
        assert!(!buf.push(Event::note_on(0, 0, 63, 1.0)));
        assert_eq!(buf.dropped(), 1);
        buf.sort();
        let keys: Vec<u8> = buf
            .as_slice()
            .iter()
            .map(|e| match e.body {
                EventBody::NoteOn { key, .. } | EventBody::NoteOff { key, .. } => key,
                _ => 0,
            })
            .collect();
        assert_eq!(keys, vec![61, 62, 60]);
    }
}
