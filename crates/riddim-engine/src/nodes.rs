//! What each kind of node does in one block. Everything here runs on the audio thread.

use riddim_model::{SlotId, TrackId};
use riddim_plugin::{Event, EventBody, EventBuffer, ProcessContext, ProcessStatus, TransportInfo};
use riddim_time::TempoMap;

use crate::dsp::strip_gains;
use crate::dsp::StereoGain;
use crate::plan::{
    MidiSourceRt, NodeRt, PluginRt, RtAudioClip, RtCurve, StepParams, TimedEvent,
    AUTOMATED_CAPACITY,
};

/// Everything about the block that is the same for every node.
pub(crate) struct BlockCtx<'a> {
    pub frames: usize,
    pub sample_rate: u32,
    pub playing: bool,
    pub recording: bool,
    /// Timeline sample of the block's first frame. Meaningful while playing.
    pub position: i64,
    /// The transport stopped or jumped since the previous block: held notes must be released.
    pub discontinuity: bool,
    /// Device input channels for the whole device block, and where this block starts in them.
    pub device_inputs: &'a [&'a [f32]],
    pub device_offset: usize,
    /// Live MIDI that arrived since the previous device block.
    pub live_midi: &'a [Event],
    /// Notes the application played since the previous device block, by track.
    pub injected: &'a [(TrackId, EventBody)],
    pub transport: &'a TransportInfo,
    pub tempo_map: &'a TempoMap,
    /// Click regardless of the metronome setting (count-in).
    pub force_click: bool,
}

/// A node's audio and event connections for one block.
pub(crate) struct NodeIo<'a> {
    pub inputs: &'a [Vec<f32>],
    pub outputs: &'a mut [Vec<f32>],
    pub events_in: &'a [Event],
    pub events_out: Option<&'a mut EventBuffer>,
    /// How late this node's input is relative to the timeline, in samples.
    pub latency: i64,
}

/// Things a node wants the engine to pass on to the control thread.
pub(crate) enum NodeMessage {
    /// A plugin changed one of its own parameters.
    PluginParam { slot: SlotId, id: u32, value: f64 },
    /// A live MIDI event to record, at an absolute timeline sample.
    RecordedMidi {
        track: TrackId,
        sample: i64,
        body: EventBody,
    },
}

pub(crate) fn process(
    rt: &mut NodeRt,
    params: &StepParams,
    io: NodeIo<'_>,
    ctx: &BlockCtx<'_>,
    mut notify: impl FnMut(NodeMessage),
) {
    let frames = ctx.frames;
    match (rt, params) {
        (
            _,
            StepParams::AudioSource {
                clips,
                monitor_input,
            },
        ) => {
            audio_source(clips, *monitor_input, io, ctx);
        }
        (
            NodeRt::MidiSource(state),
            StepParams::MidiSource {
                track,
                events,
                live_input,
                record,
            },
        ) => {
            if let Some(out) = io.events_out {
                midi_source(
                    state,
                    *track,
                    events,
                    *live_input,
                    *record,
                    out,
                    ctx,
                    &mut notify,
                );
            }
        }
        (_, StepParams::BusInput) => {
            // The executor has already summed everything routed here; pass it on.
            for (output, input) in io.outputs.iter_mut().zip(io.inputs) {
                output[..frames].copy_from_slice(&input[..frames]);
            }
        }
        (NodeRt::Plugin(plugin), StepParams::Plugin { bypass, curves }) => {
            plugin_node(plugin, *bypass, curves, io, ctx, &mut notify);
        }
        (
            NodeRt::Strip(strip),
            StepParams::Strip {
                gain,
                pan,
                mute,
                invert,
                gain_curve,
                pan_curve,
            },
        ) => {
            // Curves are read where this node's audio is on the timeline, not where the
            // playhead is: its input arrives `latency` samples late.
            let at = ctx.position - io.latency;
            let sign = if *invert { -1.0 } else { 1.0 };
            let mut gain_cursor = gain_curve.as_ref().map(|c| c.cursor());
            let mut pan_cursor = pan_curve.as_ref().map(|c| c.cursor());
            let mut gains_at = |sample: i64| {
                let gain = gain_cursor
                    .as_mut()
                    .and_then(|c| c.value_at(sample))
                    .unwrap_or(*gain);
                let pan = pan_cursor
                    .as_mut()
                    .and_then(|c| c.value_at(sample))
                    .unwrap_or(*pan);
                let (left, right) = strip_gains(gain, pan, *mute);
                (left * sign, right * sign)
            };
            let automated = ctx.playing && (gain_curve.is_some() || pan_curve.is_some());
            let meter = strip.meter.clone();
            gain_stage(
                &mut strip.gain,
                &mut strip.primed,
                automated,
                at,
                &mut gains_at,
                io,
                frames,
                |channel, peak| meter.raise(channel, peak),
            );
        }
        (NodeRt::Gain(stage), StepParams::Send { gain, mute, curve }) => {
            let at = ctx.position - io.latency;
            let mut cursor = curve.as_ref().map(|c| c.cursor());
            let mut gains_at = |sample: i64| {
                let gain = cursor
                    .as_mut()
                    .and_then(|c| c.value_at(sample))
                    .unwrap_or(*gain);
                let gain = if *mute { 0.0 } else { gain };
                (gain, gain)
            };
            let automated = ctx.playing && curve.is_some();
            gain_stage(
                &mut stage.gain,
                &mut stage.primed,
                automated,
                at,
                &mut gains_at,
                io,
                frames,
                |_, _| {},
            );
        }
        (NodeRt::Metronome(click), StepParams::Metronome { enabled, gain }) => {
            for output in io.outputs.iter_mut() {
                output[..frames].fill(0.0);
            }
            if !ctx.playing {
                click.silence();
                return;
            }
            let [left, right] = io.outputs else { return };
            if *enabled || ctx.force_click {
                // Every beat line that falls inside this block starts a click at its own sample.
                let end = ctx.position + frames as i64;
                let mut tick = ctx
                    .tempo_map
                    .samples_to_ticks(ctx.position, ctx.sample_rate);
                let mut rendered = 0;
                loop {
                    let (beat, accent) = ctx.tempo_map.next_beat(tick);
                    let at = ctx.tempo_map.ticks_to_samples(beat, ctx.sample_rate);
                    if at >= end {
                        break;
                    }
                    let offset = (at - ctx.position).max(0) as usize;
                    click.render_add(&mut left[rendered..offset], &mut right[rendered..offset]);
                    click.trigger(ctx.sample_rate, accent, *gain);
                    rendered = offset;
                    tick = riddim_time::Ticks(beat.0 + 1);
                }
                click.render_add(&mut left[rendered..frames], &mut right[rendered..frames]);
            } else {
                click.render_add(&mut left[..frames], &mut right[..frames]);
            }
        }
        (_, StepParams::Output) => {}
        // A cell and its parameters are always created together; a mismatch would be a bug in
        // plan building. Produce silence rather than garbage.
        _ => {
            for output in io.outputs.iter_mut() {
                output[..frames].fill(0.0);
            }
        }
    }
}

fn audio_source(
    clips: &[RtAudioClip],
    monitor_input: Option<(usize, usize)>,
    io: NodeIo<'_>,
    ctx: &BlockCtx<'_>,
) {
    let frames = ctx.frames;
    for output in io.outputs.iter_mut() {
        output[..frames].fill(0.0);
    }

    if ctx.playing {
        let block_start = ctx.position;
        let block_end = block_start + frames as i64;
        for clip in clips {
            let clip_end = clip.start + clip.length;
            if clip_end <= block_start || clip.start >= block_end {
                continue;
            }
            let from = clip.start.max(block_start);
            let to = clip_end.min(block_end);
            let media_channels = clip.media.channels.len();
            if media_channels == 0 {
                continue;
            }
            for (c, output) in io.outputs.iter_mut().enumerate() {
                // Mono media plays on both sides; extra media channels are ignored.
                let source = &clip.media.channels[c.min(media_channels - 1)];
                for timeline in from..to {
                    let into_clip = timeline - clip.start;
                    let Some(&sample) = source.get((clip.offset + into_clip) as usize) else {
                        break;
                    };
                    let mut gain = clip.gain;
                    if into_clip < clip.fade_in {
                        gain *= into_clip as f32 / clip.fade_in as f32;
                    }
                    let from_end = clip.length - into_clip;
                    if from_end <= clip.fade_out {
                        gain *= from_end as f32 / clip.fade_out as f32;
                    }
                    output[(timeline - block_start) as usize] += sample * gain;
                }
            }
        }
    }

    if let Some((first, channels)) = monitor_input {
        if channels > 0 {
            for (c, output) in io.outputs.iter_mut().enumerate() {
                // A mono input is heard on both sides.
                let Some(input) = ctx.device_inputs.get(first + c.min(channels - 1)) else {
                    continue;
                };
                let input = &input[ctx.device_offset..ctx.device_offset + frames];
                for (o, i) in output[..frames].iter_mut().zip(input) {
                    *o += *i;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn midi_source(
    state: &mut MidiSourceRt,
    track: TrackId,
    events: &[TimedEvent],
    live_input: bool,
    record: bool,
    out: &mut EventBuffer,
    ctx: &BlockCtx<'_>,
    notify: &mut impl FnMut(NodeMessage),
) {
    out.clear();

    // Release whatever is still held when playback stops or jumps.
    if (ctx.discontinuity || !ctx.playing) && state.any_held {
        for (channel, keys) in state.held.iter_mut().enumerate() {
            for (key, count) in keys.iter_mut().enumerate() {
                while *count > 0 {
                    out.push(Event::note_off(0, channel as u8, key as u8));
                    *count -= 1;
                }
            }
        }
        state.any_held = false;
    }

    if ctx.playing {
        let start = ctx.position;
        let end = start + ctx.frames as i64;
        let first = events.partition_point(|e| e.sample < start);
        for event in &events[first..] {
            if event.sample >= end {
                break;
            }
            match event.body {
                EventBody::NoteOn { channel, key, .. } => {
                    let count = &mut state.held[(channel & 15) as usize][(key & 127) as usize];
                    *count = count.saturating_add(1);
                    state.any_held = true;
                }
                EventBody::NoteOff { channel, key, .. } => {
                    let count = &mut state.held[(channel & 15) as usize][(key & 127) as usize];
                    // A note-off whose note-on was skipped (playback started mid-note) is dropped.
                    if *count == 0 {
                        continue;
                    }
                    *count -= 1;
                }
                _ => {}
            }
            out.push(Event {
                offset: (event.sample - start) as u32,
                body: event.body,
            });
        }
    }

    // Notes played from the application sound at once, whatever the transport is doing.
    let mut unsorted = false;
    if ctx.device_offset == 0 {
        for (target, body) in ctx.injected {
            if *target == track {
                out.push(Event {
                    offset: 0,
                    body: *body,
                });
                unsorted = true;
            }
        }
    }

    if live_input {
        unsorted |= !ctx.live_midi.is_empty();
        for event in ctx.live_midi {
            // Live input is stamped relative to the device block; deliver each event in the
            // segment of the block it falls in.
            let at = event.offset as usize;
            if at < ctx.device_offset || at >= ctx.device_offset + ctx.frames {
                continue;
            }
            let offset = (at - ctx.device_offset) as u32;
            out.push(Event {
                offset,
                body: event.body,
            });
            if record && ctx.playing && ctx.recording {
                notify(NodeMessage::RecordedMidi {
                    track,
                    sample: ctx.position + offset as i64,
                    body: event.body,
                });
            }
        }
    }
    if unsorted {
        out.sort();
    }
}

fn plugin_node(
    plugin: &mut PluginRt,
    bypass: bool,
    curves: &[(u32, std::sync::Arc<RtCurve>)],
    io: NodeIo<'_>,
    ctx: &BlockCtx<'_>,
    notify: &mut impl FnMut(NodeMessage),
) {
    let frames = ctx.frames;
    if bypass {
        if !plugin.was_bypassed {
            plugin.was_bypassed = true;
            if let Some(delay) = plugin.bypass_delay.as_mut() {
                delay.clear();
            }
        }
        // Pass the main input through to the main output (an instrument has none, and is
        // silent), delayed by the plugin's latency so the rest of the mix stays aligned with
        // it. The sidechain goes nowhere and the extra outputs are silent.
        for (c, output) in io.outputs.iter_mut().enumerate() {
            output[..frames].fill(0.0);
            if c >= plugin.main_out || c >= plugin.main_in {
                continue;
            }
            let Some(input) = io.inputs.get(c) else {
                continue;
            };
            match plugin.bypass_delay.as_mut() {
                Some(delay) => delay.process_add(c, &input[..frames], &mut output[..frames]),
                None => output[..frames].copy_from_slice(&input[..frames]),
            }
        }
        if let Some(delay) = plugin.bypass_delay.as_mut() {
            delay.advance(frames);
        }
        plugin.pending_params.clear();
        return;
    }
    plugin.was_bypassed = false;

    // Parameter changes first (they apply from the start of the block), then automation, then
    // MIDI.
    plugin.events_in.clear();
    for event in plugin.pending_params.as_slice() {
        plugin.events_in.push(*event);
    }
    plugin.pending_params.clear();

    let mut unsorted = false;
    if ctx.playing {
        let start = ctx.position - io.latency;
        let end = start + frames as i64;
        for (id, curve) in curves.iter() {
            let Some(value) = curve.value_at(start) else {
                continue;
            };
            // Send the block's starting value only when it moved (or after a jump), so a flat
            // lane costs the plugin nothing.
            let known = plugin.automated.iter().position(|(known, _)| known == id);
            let changed = match known {
                Some(index) => {
                    let moved = plugin.automated[index].1 != value;
                    plugin.automated[index].1 = value;
                    moved
                }
                None => {
                    if plugin.automated.len() < AUTOMATED_CAPACITY {
                        plugin.automated.push((*id, value));
                    }
                    true
                }
            };
            if changed || ctx.discontinuity {
                plugin.events_in.push(Event::param(0, *id, value as f64));
            }
            // Breakpoints inside the block land on their own sample.
            for point in curve.points_inside(start, end) {
                plugin.events_in.push(Event::param(
                    (point.sample - start) as u32,
                    *id,
                    point.value as f64,
                ));
                if let Some((_, last)) = plugin.automated.iter_mut().find(|(known, _)| known == id)
                {
                    *last = point.value;
                }
                unsorted = true;
            }
        }
    }
    for event in io.events_in {
        plugin.events_in.push(*event);
    }
    if unsorted {
        plugin.events_in.sort();
    }
    plugin.events_out.clear();

    let (inputs, outputs) = plugin.scratch.fill(
        io.inputs.iter().map(|c| c.as_slice()),
        io.outputs.iter_mut().map(|c| c.as_mut_slice()),
        frames,
    );
    let mut context = ProcessContext {
        frames,
        inputs,
        outputs,
        events_in: plugin.events_in.as_slice(),
        events_out: &mut plugin.events_out,
        transport: ctx.transport,
    };
    let status = plugin.processor.process(&mut context);
    if status == ProcessStatus::Failed {
        for output in io.outputs.iter_mut() {
            output[..frames].fill(0.0);
        }
    }

    let mut events_out = io.events_out;
    if let Some(out) = events_out.as_deref_mut() {
        out.clear();
    }
    for event in plugin.events_out.as_slice() {
        match event.body {
            EventBody::Param { id, value } => {
                notify(NodeMessage::PluginParam {
                    slot: plugin.slot,
                    id,
                    value,
                });
            }
            _ => {
                if let Some(out) = events_out.as_deref_mut() {
                    out.push(*event);
                }
            }
        }
    }
}

/// A stereo gain stage: copy the input to the output and apply gains.
///
/// `gains_at(sample)` gives the gains for a timeline sample and is only ever asked for
/// increasing samples. While `automated`, it is asked for every sample of the block and its
/// answer is applied directly: the automation curve is the smoothing, exact at its breakpoints
/// and independent of block size. Otherwise it is asked once, and a change from the previous
/// block ramps over the stage's own short time so a fader move does not click.
#[allow(clippy::too_many_arguments)]
fn gain_stage(
    gain: &mut StereoGain,
    primed: &mut bool,
    automated: bool,
    at: i64,
    gains_at: &mut dyn FnMut(i64) -> (f32, f32),
    io: NodeIo<'_>,
    frames: usize,
    mut meter: impl FnMut(usize, f32),
) {
    let [left, right] = io.outputs else { return };
    for (output, input) in [
        (&mut *left, io.inputs.first()),
        (&mut *right, io.inputs.get(1)),
    ] {
        match input {
            Some(input) => output[..frames].copy_from_slice(&input[..frames]),
            None => output[..frames].fill(0.0),
        }
    }

    if automated {
        let mut last = (0.0, 0.0);
        for (i, (l, r)) in left[..frames]
            .iter_mut()
            .zip(right[..frames].iter_mut())
            .enumerate()
        {
            last = gains_at(at + i as i64);
            *l *= last.0;
            *r *= last.1;
        }
        // If automation stops, the stage carries on from where the curve left it.
        gain.snap(last);
    } else {
        let now = gains_at(at);
        if *primed {
            gain.set(now);
        } else {
            gain.snap(now);
        }
        gain.apply(&mut left[..frames], &mut right[..frames]);
    }
    *primed = true;

    for (channel, output) in [&*left, &*right].into_iter().enumerate() {
        let peak = output[..frames].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        meter(channel, peak);
    }
}
