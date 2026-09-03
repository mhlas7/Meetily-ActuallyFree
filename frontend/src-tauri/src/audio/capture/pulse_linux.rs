// Native PipeWire/PulseAudio system-audio capture for Linux.
//
// Bypasses cpal's ALSA-only host entirely by talking to the PulseAudio client
// protocol directly (which PipeWire also implements via pipewire-pulse). This
// avoids the need to hand-register monitor sources as named ALSA pseudo-devices
// in ~/.asoundrc: sink descriptions and monitor sources come straight from the
// server's real metadata.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::introspect::ServerInfo;
use libpulse_binding::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::operation::{Operation, State as OperationState};
use libpulse_binding::proplist::Proplist;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use log::{info, warn};

/// A PulseAudio sink, with the metadata needed to offer it as a "System Audio"
/// capture device: a real display description and its monitor source name.
#[derive(Debug, Clone)]
pub struct PulseSink {
    pub description: String,
    /// The sink's own server name, e.g.
    /// "alsa_output.pci-0000_c1_00.6.HiFi__Speaker__sink". Distinct from
    /// `monitor_source_name` (that same sink's ".monitor" source, which is what
    /// we actually capture from); this is what `ServerInfo::default_sink_name`
    /// reports, so it is needed to identify the server's default output.
    pub sink_name: String,
    pub monitor_source_name: String,
    /// What the picker shows. Equal to `description`, except when several sinks
    /// share one description — then the server name is appended so the rows are
    /// distinguishable and each maps to exactly one sink.
    pub label: String,
}

/// Fixed capture format requested from the server. PulseAudio/PipeWire
/// transparently resamples and remixes the monitor source to this spec
/// server-side, so the rest of the pipeline (which expects 48kHz) never needs
/// to know the sink's native sample rate or channel count.
const CAPTURE_SAMPLE_RATE: u32 = 48000;

/// Build display labels, appending the server name whenever several entries
/// share a description.
///
/// Descriptions are not unique: two identical USB headsets, or several HDMI
/// outputs on one GPU, are routinely described identically. Without this the
/// picker shows duplicate rows and resolution silently binds to whichever the
/// server happened to list first. Square brackets are used rather than
/// parentheses so the label can never collide with the " (System Audio)" /
/// " (output)" suffixes the device layer appends and strips.
fn build_labels(entries: &[(String, String)]) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (description, _) in entries {
        *counts.entry(description.as_str()).or_insert(0) += 1;
    }

    entries
        .iter()
        .map(|(description, name)| {
            if counts.get(description.as_str()).copied().unwrap_or(0) > 1 {
                format!("{} [{}]", description, name)
            } else {
                description.clone()
            }
        })
        .collect()
}

/// Picker label of the sink the server reports as its default output.
///
/// Matches the sink's own name, falling back to the conventional
/// "<sink>.monitor" mapping for servers that omit `SinkInfo::name`. Returns
/// `None` when the default sink isn't in the list — it may have no monitor
/// source, in which case `list_sinks` filtered it out and we cannot capture it.
///
/// Pure so it is unit-testable without a PulseAudio server.
fn label_for_sink_name(sinks: &[PulseSink], default_sink_name: &str) -> Option<String> {
    if default_sink_name.is_empty() {
        return None;
    }

    sinks
        .iter()
        .find(|sink| sink.sink_name == default_sink_name)
        .or_else(|| {
            let monitor = format!("{}.monitor", default_sink_name);
            sinks.iter().find(|sink| sink.monitor_source_name == monitor)
        })
        .map(|sink| sink.label.clone())
}

/// Resolve a stored device string to its PulseAudio name.
///
/// Tried in order: the disambiguated label (what the picker stores today), then
/// the bare description (so preferences written before labels were
/// disambiguated keep resolving). `entries` are `(label, description, name)`.
///
/// Note both keys are display strings, and neither is a stable identity: a
/// locale change or a profile switch moves the description, and a saved
/// preference stops resolving. Fixing that needs an opaque id persisted
/// alongside the label, which is a preferences-schema change rather than
/// something this function can paper over.
fn resolve_stored(entries: &[(String, String, String)], stored: &str, kind: &str) -> Result<String> {
    if let Some((_, _, name)) = entries.iter().find(|(label, _, _)| label == stored) {
        return Ok(name.clone());
    }

    let matched: Vec<&(String, String, String)> = entries
        .iter()
        .filter(|(_, description, _)| description == stored)
        .collect();

    match matched.len() {
        0 => Err(anyhow!("No PulseAudio {} found matching '{}'", kind, stored)),
        1 => Ok(matched[0].2.clone()),
        n => {
            warn!(
                "🔊 {} PulseAudio {}s share the description '{}'; using '{}'. Re-select the device in Settings to pin it.",
                n, kind, stored, matched[0].2
            );
            Ok(matched[0].2.clone())
        }
    }
}

/// Upper bound on any single blocking PulseAudio interaction.
///
/// `Mainloop::iterate(true)` blocks until the server sends something, so a
/// server that accepts the connection but never completes the handshake — a
/// dead `PULSE_SERVER` TCP endpoint, a stalled autospawn, a sandbox where the
/// socket exists but is not wired through — would hang the caller forever.
/// `configure_linux_audio()` runs on the device-monitor poll every few seconds
/// while a recording is active, so an unbounded wait there stalls device
/// monitoring rather than just one call. Every wait below is therefore
/// deadline-bounded and iterates the mainloop non-blocking.
const PULSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to sleep when a non-blocking mainloop iteration had nothing to do.
const PULSE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Iterate the mainloop without blocking, returning how many events were
/// dispatched. `while_doing` names the operation for error messages.
fn pump(mainloop: &mut Mainloop, while_doing: &str) -> Result<u32> {
    match mainloop.iterate(false) {
        IterateResult::Quit(_) | IterateResult::Err(_) => Err(anyhow!(
            "PulseAudio mainloop iteration failed while {}",
            while_doing
        )),
        IterateResult::Success(dispatched) => Ok(dispatched),
    }
}

/// Pump the mainloop until `operation` finishes or `PULSE_TIMEOUT` elapses.
fn run_operation_to_completion<T: ?Sized>(
    mainloop: &mut Mainloop,
    operation: &Operation<T>,
) -> Result<()> {
    let deadline = Instant::now() + PULSE_TIMEOUT;

    loop {
        match operation.get_state() {
            OperationState::Done => return Ok(()),
            OperationState::Cancelled => {
                return Err(anyhow!("PulseAudio operation was cancelled"));
            }
            OperationState::Running => {}
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "PulseAudio operation did not complete within {:?}",
                PULSE_TIMEOUT
            ));
        }

        if pump(mainloop, "waiting for an operation to complete")? == 0 {
            std::thread::sleep(PULSE_POLL_INTERVAL);
        }
    }
}

/// Connect a fresh context to the default PulseAudio/PipeWire server, waiting
/// up to `PULSE_TIMEOUT` for it to become ready. Each call opens (and the caller
/// later drops) its own connection — unlike ALSA's cached global config, there's
/// no stale-state problem to work around here, so every call sees the server's
/// current sinks.
fn connect() -> Result<(Mainloop, Context)> {
    info!("🔊 pulse_linux::connect: creating mainloop");
    let mut mainloop =
        Mainloop::new().ok_or_else(|| anyhow!("Failed to create PulseAudio mainloop"))?;

    let proplist =
        Proplist::new().ok_or_else(|| anyhow!("Failed to create PulseAudio proplist"))?;
    let mut context = Context::new_with_proplist(&mainloop, "Meetily", &proplist)
        .ok_or_else(|| anyhow!("Failed to create PulseAudio context"))?;

    info!("🔊 pulse_linux::connect: calling context.connect()");
    context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .map_err(|e| anyhow!("Failed to connect to PulseAudio/PipeWire server: {}", e))?;

    info!("🔊 pulse_linux::connect: waiting for context to become Ready");
    let deadline = Instant::now() + PULSE_TIMEOUT;

    loop {
        match context.get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                return Err(anyhow!(
                    "PulseAudio/PipeWire context connection failed or was terminated"
                ));
            }
            _ => {}
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "Timed out after {:?} waiting for the PulseAudio/PipeWire server to become ready (last state: {:?})",
                PULSE_TIMEOUT,
                context.get_state()
            ));
        }

        if pump(&mut mainloop, "connecting")? == 0 {
            std::thread::sleep(PULSE_POLL_INTERVAL);
        }
    }
    info!("🔊 pulse_linux::connect: context Ready");

    Ok((mainloop, context))
}

/// List all sinks (playback outputs) with their monitor source name, for use as
/// "System Audio" capture devices. Descriptions come straight from the server —
/// no manual ~/.asoundrc registration needed, and switching outputs (new DAC,
/// different Bluetooth device) is picked up on the next call since nothing here
/// is cached between calls.
pub fn list_sinks() -> Result<Vec<PulseSink>> {
    info!("🔊 pulse_linux::list_sinks: connecting");
    let (mut mainloop, context) = connect()?;

    info!("🔊 pulse_linux::list_sinks: connected, requesting sink list");
    // (description, sink_name, monitor_source_name) triples; labels are assigned
    // after the full list is known, since disambiguation depends on the other
    // entries.
    let sinks: Rc<RefCell<Vec<(String, String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let sinks_cb = sinks.clone();

    let operation = context.introspect().get_sink_info_list(move |result| {
        if let ListResult::Item(info) = result {
            let monitor_source_name = info.monitor_source_name.as_deref().unwrap_or_default();
            if monitor_source_name.is_empty() {
                return;
            }

            let description = info
                .description
                .as_deref()
                .unwrap_or("Unknown output")
                .to_string();

            let sink_name = info.name.as_deref().unwrap_or_default().to_string();

            sinks_cb.borrow_mut().push((
                description,
                sink_name,
                monitor_source_name.to_string(),
            ));
        }
    });

    info!("🔊 pulse_linux::list_sinks: pumping mainloop until sink list operation completes");
    run_operation_to_completion(&mut mainloop, &operation)?;
    drop(operation);

    let raw = sinks.borrow().clone();

    // Deliberately keep feeding build_labels the monitor name, not sink_name:
    // the bracketed discriminator it produces is persisted in user preferences,
    // so changing it would silently orphan saved selections for devices with
    // duplicate descriptions.
    let label_input: Vec<(String, String)> = raw
        .iter()
        .map(|(description, _, monitor)| (description.clone(), monitor.clone()))
        .collect();
    let labels = build_labels(&label_input);

    let result: Vec<PulseSink> = raw
        .into_iter()
        .zip(labels)
        .map(|((description, sink_name, monitor_source_name), label)| PulseSink {
            description,
            sink_name,
            monitor_source_name,
            label,
        })
        .collect();

    info!("🔊 pulse_linux::list_sinks: got {} sink(s)", result.len());
    Ok(result)
}

/// Resolve a stored "System Audio" device string to its sink's monitor source
/// name. Accepts the picker label, or a bare description saved by an older
/// build.
pub fn resolve_monitor_source(stored: &str) -> Result<String> {
    let entries: Vec<(String, String, String)> = list_sinks()?
        .into_iter()
        .map(|sink| (sink.label, sink.description, sink.monitor_source_name))
        .collect();

    let resolved = resolve_stored(&entries, stored, "sink")?;
    info!("🔍 resolve_monitor_source: '{}' -> '{}'", stored, resolved);
    Ok(resolved)
}

/// Picker label of the server's current default output sink, if any.
///
/// Used to resolve "Default System Audio" to whatever output the user is
/// actually listening on — Bluetooth, jack, or built-in — rather than pinning
/// one specific sink.
pub fn default_sink_label() -> Result<Option<String>> {
    info!("🔊 pulse_linux::default_sink_label: connecting");
    let (mut mainloop, context) = connect()?;

    info!("🔊 pulse_linux::default_sink_label: requesting server info");
    let default_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let default_name_cb = default_name.clone();

    let operation = context.introspect().get_server_info(move |info: &ServerInfo| {
        if let Some(name) = info.default_sink_name.as_deref() {
            *default_name_cb.borrow_mut() = Some(name.to_string());
        }
    });

    run_operation_to_completion(&mut mainloop, &operation)?;
    drop(operation);

    let default_name = default_name.borrow().clone();
    let Some(default_name) = default_name else {
        warn!("🔊 pulse_linux::default_sink_label: server reported no default sink");
        return Ok(None);
    };

    info!(
        "🔊 pulse_linux::default_sink_label: default sink name is '{}'",
        default_name
    );

    let sinks = list_sinks()?;
    match label_for_sink_name(&sinks, &default_name) {
        Some(label) => {
            info!(
                "🔊 pulse_linux::default_sink_label: default sink label is '{}'",
                label
            );
            Ok(Some(label))
        }
        None => {
            warn!(
                "🔊 pulse_linux::default_sink_label: default sink '{}' has no capturable monitor source",
                default_name
            );
            Ok(None)
        }
    }
}

/// A PulseAudio input source (microphone, line-in, …), excluding sink monitors.
#[derive(Debug, Clone)]
pub struct PulseSource {
    /// Human-readable description straight from the server — this is the exact
    /// string KDE's own audio settings show (e.g. "Ryzen HD Audio Controller
    /// Headset Mono Microphone").
    pub description: String,
    /// Real PulseAudio source name, e.g.
    /// "alsa_input.pci-0000_c1_00.6.HiFi__Headset__source".
    pub source_name: String,
    /// What the picker shows. See `PulseSink::label`.
    pub label: String,
}

/// List all real input sources (sink monitors excluded — those are offered as
/// "System Audio" devices via `list_sinks`).
pub fn list_sources() -> Result<Vec<PulseSource>> {
    info!("🎤 pulse_linux::list_sources: connecting");
    let (mut mainloop, context) = connect()?;

    info!("🎤 pulse_linux::list_sources: connected, requesting source list");
    // (description, source_name) pairs; labels assigned after the full list is
    // known. See list_sinks().
    let sources: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let sources_cb = sources.clone();

    let operation = context.introspect().get_source_info_list(move |result| {
        if let ListResult::Item(info) = result {
            // Monitors of sinks are already exposed as "System Audio" devices
            // through list_sinks(); ignore them here.
            if info.monitor_of_sink.is_some() {
                return;
            }

            let source_name = info.name.as_deref().unwrap_or_default();
            if source_name.is_empty() {
                return;
            }

            let description = info
                .description
                .as_deref()
                .unwrap_or(source_name)
                .to_string();

            sources_cb
                .borrow_mut()
                .push((description, source_name.to_string()));
        }
    });

    info!("🎤 pulse_linux::list_sources: pumping mainloop until source list operation completes");
    run_operation_to_completion(&mut mainloop, &operation)?;
    drop(operation);

    let raw = sources.borrow().clone();
    let labels = build_labels(&raw);
    let result: Vec<PulseSource> = raw
        .into_iter()
        .zip(labels)
        .map(|((description, source_name), label)| PulseSource {
            description,
            source_name,
            label,
        })
        .collect();

    info!("🎤 pulse_linux::list_sources: got {} source(s)", result.len());
    Ok(result)
}

/// Resolve a stored microphone device string to its PulseAudio source name.
/// Accepts the picker label, or a bare description saved by an older build.
pub fn resolve_source(stored: &str) -> Result<String> {
    let entries: Vec<(String, String, String)> = list_sources()?
        .into_iter()
        .map(|source| (source.label, source.description, source.source_name))
        .collect();

    let resolved = resolve_stored(&entries, stored, "source")?;
    info!("🎤 resolve_source: '{}' -> '{}'", stored, resolved);
    Ok(resolved)
}

/// Picker label of the server's default input source, if any.
/// Used to resolve "Default Microphone".
pub fn default_source_label() -> Result<Option<String>> {
    info!("🎤 pulse_linux::default_source_label: connecting");
    let (mut mainloop, context) = connect()?;

    info!("🎤 pulse_linux::default_source_label: requesting server info");
    let default_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let default_name_cb = default_name.clone();

    let operation = context.introspect().get_server_info(move |info: &ServerInfo| {
        if let Some(name) = info.default_source_name.as_deref() {
            *default_name_cb.borrow_mut() = Some(name.to_string());
        }
    });

    run_operation_to_completion(&mut mainloop, &operation)?;
    drop(operation);

    let default_name = default_name.borrow().clone();
    let Some(default_name) = default_name else {
        return Ok(None);
    };

    info!(
        "🎤 pulse_linux::default_source_label: default source name is '{}'",
        default_name
    );

    // Resolve the display description for the default source. Do a second
    // introspection call: it's rare (startup of a recording) and keeps the code
    // straightforward.
    let sources = list_sources()?;
    let default_source = sources.iter().find(|s| s.source_name == default_name);

    if let Some(source) = default_source {
        info!(
            "🎤 pulse_linux::default_source_label: default source label is '{}'",
            source.label
        );
        Ok(Some(source.label.clone()))
    } else {
        warn!(
            "🎤 pulse_linux::default_source_label: default source '{}' not found in source list",
            default_name
        );
        Ok(None)
    }
}

/// Blocking PulseAudio record stream, backed by libpulse-simple's record API.
/// Used both for sink monitors (system audio) and real input sources
/// (microphones).
pub struct PulseCapture {
    simple: Simple,
    should_stop: Arc<AtomicBool>,
    channels: u16,
}

impl PulseCapture {
    fn new_with_channels(source_name: &str, channels: u16, stream_label: &str) -> Result<Self> {
        let spec = Spec {
            format: Format::FLOAT32NE,
            channels: channels as u8,
            rate: CAPTURE_SAMPLE_RATE,
        };
        if !spec.is_valid() {
            return Err(anyhow!("Invalid PulseAudio sample spec"));
        }

        // Match fragsize exactly to our read buffer (1024 frames) to prevent timing issues.
        // This ensures PulseAudio delivers exactly what we're reading each iteration.
        use libpulse_binding::def::BufferAttr;

        const FRAMES_PER_CHUNK: u32 = 1024; // must match the constant in run()
        let fragsize = FRAMES_PER_CHUNK * channels as u32 * 4; // bytes per chunk
        let maxlength = (CAPTURE_SAMPLE_RATE / 2) * channels as u32 * 4; // 500ms max buffer

        let buffer_attr = BufferAttr {
            maxlength,      // 500ms max buffer to survive CPU spikes
            tlength: std::u32::MAX,   // not used for record streams
            prebuf: std::u32::MAX,    // not used for record streams
            minreq: std::u32::MAX,    // not used for record streams
            fragsize,       // exactly match our read buffer size
        };

        info!("🔊 PulseAudio buffer: fragsize=1024 frames (~21ms), maxlength={}ms",
              maxlength as f32 / (CAPTURE_SAMPLE_RATE * channels as u32 * 4) as f32 * 1000.0);

        let simple = Simple::new(
            None, // default server
            "Meetily",
            Direction::Record,
            Some(source_name),
            stream_label,
            &spec,
            None, // default channel map
            Some(&buffer_attr), // large max buffer, default timing
        )
        .map_err(|e| {
            anyhow!(
                "Failed to open PulseAudio record stream on '{}': {}",
                source_name,
                e
            )
        })?;

        Ok(Self {
            simple,
            should_stop: Arc::new(AtomicBool::new(false)),
            channels,
        })
    }

    /// System audio: stereo capture of a sink's monitor source.
    pub fn new_system(monitor_source_name: &str) -> Result<Self> {
        Self::new_with_channels(monitor_source_name, 2, "System Audio")
    }

    /// Microphone: mono capture of a real input source. PulseAudio downmixes and
    /// resamples server-side, so the pipeline still receives 48kHz.
    pub fn new_microphone(source_name: &str) -> Result<Self> {
        Self::new_with_channels(source_name, 1, "Microphone")
    }

    pub fn sample_rate(&self) -> u32 {
        CAPTURE_SAMPLE_RATE
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Handle used to signal the capture loop (running on another thread) to stop.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        self.should_stop.clone()
    }

    /// Runs a blocking read loop, invoking `on_samples` with interleaved f32
    /// frames as they arrive. Meant to run on a `spawn_blocking` task, since
    /// PulseAudio's simple API blocks on `read()`. Returns once `stop_handle()`
    /// is signalled or the stream errors out.
    pub fn run(&self, mut on_samples: impl FnMut(&[f32])) {
        // ~21ms at 48kHz stereo: matches the 1024-frame chunking used by the
        // macOS Core Audio path.
        const FRAMES_PER_CHUNK: usize = 1024;
        let mut byte_buf = vec![0u8; FRAMES_PER_CHUNK * self.channels as usize * 4];
        let mut sample_buf = Vec::with_capacity(FRAMES_PER_CHUNK * self.channels as usize);

        while !self.should_stop.load(Ordering::Acquire) {
            if let Err(e) = self.simple.read(&mut byte_buf) {
                warn!("PulseAudio record stream read error: {}", e);
                break;
            }

            // Re-check right after the blocking read returns: if stop() timed
            // out waiting for this thread (stalled source) and moved on, a
            // late read() shouldn't inject one more chunk into whatever
            // pipeline state now exists (new stream, new recording).
            if self.should_stop.load(Ordering::Acquire) {
                break;
            }

            sample_buf.clear();
            sample_buf.extend(
                byte_buf
                    .chunks_exact(4)
                    .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]])),
            );

            on_samples(&sample_buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(description: &str, sink_name: &str, monitor: &str, label: &str) -> PulseSink {
        PulseSink {
            description: description.to_string(),
            sink_name: sink_name.to_string(),
            monitor_source_name: monitor.to_string(),
            label: label.to_string(),
        }
    }

    #[test]
    fn test_label_for_sink_name_matches_sink_name() {
        let sinks = vec![
            sink("Speakers", "alsa_output.pci-0000", "alsa_output.pci-0000.monitor", "Speakers"),
            sink("JBL Tune 770NC", "bluez_output.AC_12", "bluez_output.AC_12.monitor", "JBL Tune 770NC"),
        ];
        assert_eq!(
            label_for_sink_name(&sinks, "bluez_output.AC_12"),
            Some("JBL Tune 770NC".to_string())
        );
    }

    #[test]
    fn test_label_for_sink_name_falls_back_to_monitor_suffix() {
        // A server that omits SinkInfo::name still resolves via the
        // conventional "<sink>.monitor" naming.
        let sinks = vec![sink("Speakers", "", "alsa_output.pci-0000.monitor", "Speakers")];
        assert_eq!(
            label_for_sink_name(&sinks, "alsa_output.pci-0000"),
            Some("Speakers".to_string())
        );
    }

    #[test]
    fn test_label_for_sink_name_prefers_sink_name_over_monitor_suffix() {
        // The suffix rule alone would pick the wrong entry here; pins precedence.
        let sinks = vec![
            sink("Wrong", "other_sink", "alsa_output.pci-0000.monitor", "Wrong"),
            sink("Right", "alsa_output.pci-0000", "some_other.monitor", "Right"),
        ];
        assert_eq!(
            label_for_sink_name(&sinks, "alsa_output.pci-0000"),
            Some("Right".to_string())
        );
    }

    #[test]
    fn test_label_for_sink_name_returns_none_when_absent() {
        // The disconnected-Bluetooth case: server still names a default sink
        // that is no longer in the list.
        let sinks = vec![sink("Speakers", "alsa_output.pci-0000", "alsa_output.pci-0000.monitor", "Speakers")];
        assert_eq!(label_for_sink_name(&sinks, "bluez_output.GONE"), None);
    }

    #[test]
    fn test_label_for_sink_name_returns_none_for_empty_list() {
        assert_eq!(label_for_sink_name(&[], "alsa_output.pci-0000"), None);
    }

    #[test]
    fn test_label_for_sink_name_returns_none_for_empty_default() {
        // An empty default name must not match a sink that also has none.
        let sinks = vec![sink("Speakers", "", "alsa_output.pci-0000.monitor", "Speakers")];
        assert_eq!(label_for_sink_name(&sinks, ""), None);
    }

    fn entry(label: &str, description: &str, name: &str) -> (String, String, String) {
        (label.to_string(), description.to_string(), name.to_string())
    }

    #[test]
    fn test_build_labels_leaves_unique_descriptions_alone() {
        let entries = vec![
            ("Built-in Analog Stereo".to_string(), "alsa_output.pci-0000_00_1f.3".to_string()),
            ("JBL Tune 770NC".to_string(), "bluez_output.AC_12_2F".to_string()),
        ];
        assert_eq!(
            build_labels(&entries),
            vec!["Built-in Analog Stereo", "JBL Tune 770NC"]
        );
    }

    #[test]
    fn test_build_labels_disambiguates_shared_descriptions() {
        let entries = vec![
            ("USB Headset".to_string(), "alsa_output.usb-0001".to_string()),
            ("USB Headset".to_string(), "alsa_output.usb-0002".to_string()),
            ("Built-in".to_string(), "alsa_output.pci-0000".to_string()),
        ];
        assert_eq!(
            build_labels(&entries),
            vec![
                "USB Headset [alsa_output.usb-0001]",
                "USB Headset [alsa_output.usb-0002]",
                "Built-in",
            ]
        );
    }

    #[test]
    fn test_build_labels_uses_brackets_not_parens() {
        // Parentheses would collide with the " (System Audio)" / " (output)"
        // suffixes the device layer appends and later strips.
        let entries = vec![
            ("Dock".to_string(), "sink_a".to_string()),
            ("Dock".to_string(), "sink_b".to_string()),
        ];
        for label in build_labels(&entries) {
            assert!(!label.ends_with(')'), "label '{}' must not end with a paren", label);
        }
    }

    #[test]
    fn test_resolve_stored_matches_label_first() {
        let entries = vec![
            entry("USB Headset [sink_a]", "USB Headset", "sink_a"),
            entry("USB Headset [sink_b]", "USB Headset", "sink_b"),
        ];
        assert_eq!(
            resolve_stored(&entries, "USB Headset [sink_b]", "sink").unwrap(),
            "sink_b"
        );
    }

    #[test]
    fn test_resolve_stored_falls_back_to_bare_description() {
        // A preference saved before labels were disambiguated.
        let entries = vec![entry("Built-in", "Built-in", "alsa_output.pci-0000")];
        assert_eq!(
            resolve_stored(&entries, "Built-in", "sink").unwrap(),
            "alsa_output.pci-0000"
        );
    }

    #[test]
    fn test_resolve_stored_is_deterministic_when_descriptions_collide() {
        let entries = vec![
            entry("USB Headset [sink_a]", "USB Headset", "sink_a"),
            entry("USB Headset [sink_b]", "USB Headset", "sink_b"),
        ];
        // Ambiguous, but must pick the first listed rather than fail.
        assert_eq!(
            resolve_stored(&entries, "USB Headset", "sink").unwrap(),
            "sink_a"
        );
    }

    #[test]
    fn test_resolve_stored_errors_when_nothing_matches() {
        let entries = vec![entry("Built-in", "Built-in", "alsa_output.pci-0000")];
        assert!(resolve_stored(&entries, "Vanished Device", "sink").is_err());
    }


    #[test]
    #[ignore] // Requires a running PulseAudio/PipeWire server; run manually.
    fn test_list_sinks() {
        let sinks = list_sinks().expect("Failed to list sinks");
        for sink in &sinks {
            println!("sink: {} -> monitor {}", sink.description, sink.monitor_source_name);
        }
        assert!(!sinks.is_empty(), "Expected at least one sink on a machine with audio output");
    }

    #[test]
    #[ignore] // Requires a running PulseAudio/PipeWire server; run manually.
    fn test_list_sources() {
        let sources = list_sources().expect("Failed to list sources");
        for source in &sources {
            println!("source: {} -> {}", source.description, source.source_name);
        }
        assert!(!sources.is_empty(), "Expected at least one real input source on a machine with audio input");
    }

    #[test]
    #[ignore] // Requires real audio playback during the test; run manually.
    fn test_capture_reads_nonzero_samples() {
        let sinks = list_sinks().expect("Failed to list sinks");
        let sink = sinks
            .into_iter()
            .find(|s| s.monitor_source_name.to_lowercase().contains("bluez")
                || s.monitor_source_name.to_lowercase().contains("running"))
            .or_else(|| list_sinks().unwrap().into_iter().next())
            .expect("No sink available");

        println!("Capturing from: {} ({})", sink.description, sink.monitor_source_name);
        let capture = PulseCapture::new_system(&sink.monitor_source_name).expect("Failed to open capture");
        let stop = capture.stop_handle();

        let received = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let received_clone = received.clone();
        let stop_clone = stop.clone();
        let handle = std::thread::spawn(move || {
            capture.run(|samples| {
                received_clone.fetch_add(samples.len(), std::sync::atomic::Ordering::Relaxed);
                if received_clone.load(std::sync::atomic::Ordering::Relaxed) > 48000 * 2 {
                    stop_clone.store(true, std::sync::atomic::Ordering::Release);
                }
            });
        });

        std::thread::sleep(std::time::Duration::from_secs(3));
        stop.store(true, std::sync::atomic::Ordering::Release);
        handle.join().unwrap();

        let total = received.load(std::sync::atomic::Ordering::Relaxed);
        println!("Received {} samples", total);
        assert!(total > 0, "Expected to receive some samples from the monitor source");
    }
}
