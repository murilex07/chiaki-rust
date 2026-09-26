// SPDX-License-Identifier: AGPL-3.0-only
//! Audio-Ausgabe — Port der SDL-Audio-Teile von `gui/src/streamsession.cpp`
//! (`InitAudio`, `PushAudioFrame`, `QueueAudioOutData`,
//! `DrainAudioOutRingBuffer`, `AudioOutDrainThreadMain`) auf cpal/WASAPI.
//!
//! Architektur (seit 12.09. zweistufig, wieder wie im C++): Der C++-Client
//! hatte zwei Pufferstufen — der Decoder füllt einen Ring
//! (`QueueAudioOutData`), ein Drain-Thread (`DrainAudioOutRingBuffer`/
//! `AudioOutDrainThreadMain`) kopiert daraus in die SDL-Geräte-Queue, und
//! SDLs eigener Thread verbraucht sie. Der erste Port hatte das auf EINE
//! Stufe reduziert (cpal-Callback zog direkt aus dem Decode-Ring) — das
//! reichte nicht: Unter Gameplay-Last kommen Audiopakete in Schüben, der
//! dünne Ring pendelte zwischen Fast-Leer (Crackle) und Überlauf
//! (Latenz-Guard-Schnitte; Messung 12.09.: 396 Schnitte/32 min ohne
//! Kamera). Deshalb wieder zwei Stufen:
//!
//! 1. **Decode-Ring** — der Audio-Thread pusht sofort bei Ankunft; kein
//!    Guard, nur Drop-Oldest bei Überlauf (C++ `QueueAudioOutData`).
//! 2. **Drain-Thread** — wacht bei Nachschub auf und hält den
//!    **Device-Ring** auf dem C++-Drain-Target (2×buffer); überschreitet
//!    er die Latenzgrenze (3×buffer), wirft er den kompletten Rückstand
//!    weg (C++-Guard, "Audio queue exceeded latency threshold").
//! 3. **cpal-Callback** — zieht aus dem Device-Ring; leer → Stille
//!    (Unterlauf-Zähler, Prime-Phase wie gehabt).
//!
//! Ankunftsbursts landen damit im Decode-Ring (Kapazität 8×buffer) und
//! treffen den Device-Ring nur über den glatten Drain — beides Crackling-
//! Quellen der Ein-Stufen-Version sind damit entschärft.
//!
//! Alle von `settings` kommenden Größen sind wie im C++ **Bytes** von
//! S16-PCM: Ring = `8 * audio_buffer_size`, Latenzgrenze = `3 *
//! audio_buffer_size` ("Audio queue exceeded latency threshold"), Default
//! 9600 = 50 ms @ 48 kHz stereo. Der Callback beginnt erst zu spielen,
//! wenn `2 * audio_buffer_size` im Device-Ring stehen (C++: Drain-Target
//! der Device-Queue) und geht nach jedem Unterlauf wieder in die
//! Prime-Phase — ohne das lief der Ring dauerhaft mit 10–30 ms Füllstand
//! am Unterlauf-Anschlag (Messung 06.09.).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use chiaki_core::{ChiakiError, ChiakiResult};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};

use super::{map_channels, pick_stream_config, resolve_device, Direction};

/// C++: `Settings::GetAudioBufferSizeDefault()` (settings.cpp) — Default für
/// `settings/audio_buffer_size` (Bytes S16-PCM; 9600 B = 2400 Frames = 50 ms
/// @ 48 kHz stereo).
pub const DEFAULT_AUDIO_BUFFER_SIZE: u32 = 9600;

/// C++: `SDL_MIX_MAXVOLUME` — Volume-Skala der Einstellung `settings/audio_volume`
/// (0..=128, Default 128). 128 = unverstärkt (C++: memcpy-Zweig).
pub const SDL_MIX_MAXVOLUME: u32 = 128;

// ---------------------------------------------------------------------------
// SampleRing — fester Ring mit Drop-Oldest (Port von audio_out_ring_*)
// ---------------------------------------------------------------------------

/// Fester Ring aus interleaved i16-Samples. Genau **ein** Produzent
/// (Session-Thread, `push`) und **ein** Konsument (cpal-Audio-Callback,
/// `pull`). Überlauf verwirft die ältesten Samples (wie `QueueAudioOutData`:
/// `audio_out_ring_read_pos += bytes_to_drop`), die Warnung wird wie
/// `audio_out_overflow_logged` nur einmal bis zum nächsten Leerlauf
/// ausgegeben.
///
/// Der Zustand liegt — wie im C++ (`QMutexLocker locker(&audio_out_ring_mutex)`)
/// — hinter einem Mutex; die kritischen Abschnitte sind reine Kopier-
/// Operationen im Mikrosekundenbereich. Zähler fürs Stats-HUD laufen über
/// Atomics, damit sie ohne Lock lesbar sind.
struct SampleRing {
    state: Mutex<RingState>,
    /// Zielvorfüllung in Samples, bevor der Callback zu spielen beginnt
    /// (C++: Drain-Target = 2×buffer_size; 0 = Tests).
    prefill: usize,
    /// In den Ring geschobene Samples (Producer-Gesamtmenge, Messung P1).
    pushed: AtomicU64,
    /// Vom Callback entnommene Samples (Konsument-Gesamtmenge, Messung P1).
    pulled: AtomicU64,
    /// Samples, die durch Überlauf/Latenz-Clear verworfen wurden (Stats-HUD).
    dropped: AtomicU64,
    /// Callbacks, bei denen der Ring leer war und Stille gespielt wurde.
    underflows: AtomicU64,
    /// Ausgelöste 3×-Latenz-Clears ("queue exceeded latency threshold").
    clears: AtomicU64,
}

struct RingState {
    buf: Vec<i16>,
    read: usize,
    write: usize,
    fill: usize,
    overflow_warned: bool,
    /// Prime-Phase: Der Callback spielt Stille, bis `prefill` Samples im
    /// Ring sind (C++: der Drain-Thread füllte die SDL-Device-Queue auf
    /// 2×buffer auf, BEVOR SDL sie abspielt). Nach einem Unterlauf wird
    /// neu geprimed — sonst läuft der Ring dauerhaft am Anschlag leer
    /// (gemessen 06.09.: fill 10–30 ms, Underflows steigen stetig).
    primed: bool,
}

impl SampleRing {
    /// `prefill` = Zielvorfüllung in Samples (0 = sofort spielen, Tests).
    fn new(capacity_samples: usize, prefill: usize) -> Self {
        SampleRing {
            state: Mutex::new(RingState {
                buf: vec![0i16; capacity_samples],
                read: 0,
                write: 0,
                fill: 0,
                overflow_warned: false,
                primed: prefill == 0,
            }),
            prefill,
            pushed: AtomicU64::new(0),
            pulled: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            underflows: AtomicU64::new(0),
            clears: AtomicU64::new(0),
        }
    }

    /// Aktuelle Füllmenge in Samples.
    fn fill(&self) -> u64 {
        self.state
            .lock()
            .map(|state| state.fill as u64)
            .unwrap_or(0)
    }

    /// Port von `QueueAudioOutData` + dem 3×-Latenz-Guard aus
    /// `PushAudioFrame`/`DrainAudioOutRingBuffer`:
    ///
    /// 1. `fill > 3 * buffer` → den kompletten Rückstand verwerfen
    ///    ("Audio queue exceeded latency threshold, clearing queued audio"),
    /// 2. Überlauf: älteste Samples droppen, Warnung einmalig
    ///    ("Audio output ring overflow, dropping stale queued audio"),
    /// 3. `data >= capacity`: nur das Ende behalten, Ring neu beginnen.
    fn push(&self, data: &[i16], clear_threshold: u64) {
        if data.is_empty() {
            return;
        }
        self.pushed.fetch_add(data.len() as u64, Ordering::Relaxed);
        let Ok(mut state) = self.state.lock() else {
            return; // vergifteter Lock: Audio wegwerfen, nicht blockieren
        };
        let capacity = state.buf.len();
        if capacity == 0 {
            return;
        }

        // C++ PushAudioFrame/DrainAudioOutRingBuffer: Queue-Latenz
        // überschritten → sämtliche angestauten Samples wegwerfen und mit
        // dem aktuellen Frame neu ansetzen.
        if state.fill as u64 > clear_threshold {
            self.dropped
                .fetch_add(state.fill as u64, Ordering::Relaxed);
            state.read = 0;
            state.write = 0;
            state.fill = 0;
            state.overflow_warned = false;
            state.primed = false; // Clear → neu vorfüllen (siehe pull)
            self.clears.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("Audio queue exceeded latency threshold, clearing queued audio");
        }

        // C++ QueueAudioOutData: passt der neue Frame nicht, fliegt das
        // älteste Material raus ("dropping stale queued audio").
        let mut data = data;
        if data.len() >= capacity {
            self.dropped.fetch_add(state.fill as u64, Ordering::Relaxed);
            state.read = 0;
            state.write = 0;
            state.fill = 0;
            state.overflow_warned = false;
            data = &data[data.len() - capacity..];
        } else if data.len() > capacity - state.fill {
            let drop = data.len() - (capacity - state.fill);
            state.read = (state.read + drop) % capacity;
            state.fill -= drop;
            self.dropped.fetch_add(drop as u64, Ordering::Relaxed);
            if !state.overflow_warned {
                tracing::warn!("Audio output ring overflow, dropping stale queued audio");
                state.overflow_warned = true;
            }
        }

        let start = state.write;
        let first = data.len().min(capacity - start);
        state.buf[start..start + first].copy_from_slice(&data[..first]);
        if data.len() > first {
            state.buf[..data.len() - first].copy_from_slice(&data[first..]);
        }
        state.write = (start + data.len()) % capacity;
        state.fill += data.len();
        // Vorfüllung erreicht → der Callback darf ab jetzt entnehmen.
        if state.fill >= self.prefill {
            state.primed = true;
        }
    }

    /// Port der Entnahme-Seite von `DrainAudioOutRingBuffer` (hier als
    /// Echtzeit-Callback): kopiert bis zu `out.len()` Samples heraus — in
    /// Stücken wie die C++-Drain-Schleife, inklusive Wrap-around — und lässt
    /// den Rest von `out` unberührt (der Aufrufer hat ihn mit Stille gefüllt
    /// — SDL-Verhalten bei Unterlauf). Setzt wie das C++
    /// `audio_out_overflow_logged = false` zurück, sobald der Ring leer ist.
    fn pull(&self, out: &mut [i16]) -> usize {
        let mut done = 0;
        let mut was_primed = true;
        if let Ok(mut state) = self.state.lock() {
            let capacity = state.buf.len();
            if capacity > 0 {
                // Prime-Phase (nach Start/Clear/Unterlauf): Stille spielen,
                // bis die Vorfüllung steht — verhindert das dauerhafte
                // Leerlaufen des Rings (C++: Drain füllte die Device-Queue
                // erst an, bevor sie abgespielt wurde).
                if !state.primed && state.fill < self.prefill {
                    was_primed = false;
                } else {
                    while done < out.len() && state.fill > 0 {
                        let chunk = state.fill.min(out.len() - done).min(capacity - state.read);
                        out[done..done + chunk]
                            .copy_from_slice(&state.buf[state.read..state.read + chunk]);
                        state.read = (state.read + chunk) % capacity;
                        state.fill -= chunk;
                        done += chunk;
                    }
                    // Ring leer gespielt → Unterlauf: neu vorfüllen.
                    if state.fill == 0 {
                        state.primed = false;
                        state.overflow_warned = false;
                    } else {
                        state.overflow_warned = false;
                    }
                }
            }
        }
        if done < out.len() {
            // Underflow → der Rest von `out` bleibt Stille (SDL-Verhalten).
            // Während der Prime-Phase ist Stille gewollt — kein Underflow.
            if was_primed {
                self.underflows.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.pulled.fetch_add(done as u64, Ordering::Relaxed);
        done
    }

    /// Drain-seitige Entnahme (Decode-Ring → Device-Ring): kopiert bis zu
    /// `max` Samples nach `out` (Wrap-arounds wie `pull`) — OHNE Prime-/
    /// Underflow-Logik (die gehört zum Device-Ring und dessen Callback).
    fn pop(&self, max: usize, out: &mut Vec<i16>) -> usize {
        out.clear();
        if let Ok(mut state) = self.state.lock() {
            let capacity = state.buf.len();
            let want = max.min(state.fill);
            if want > 0 {
                let first = want.min(capacity - state.read);
                out.extend_from_slice(&state.buf[state.read..state.read + first]);
                if want > first {
                    out.extend_from_slice(&state.buf[..want - first]);
                }
                state.read = (state.read + want) % capacity;
                state.fill -= want;
                state.overflow_warned = false;
            }
            return want;
        }
        0
    }

    /// C++-Queue-Clear im Drain (`SDL_GetQueuedAudioSize > 3×buffer`): den
    /// kompletten Rückstand verwerfen, Zähler/Prime zurücksetzen.
    fn clear_backlog(&self) {
        if let Ok(mut state) = self.state.lock() {
            self.dropped
                .fetch_add(state.fill as u64, Ordering::Relaxed);
            state.read = 0;
            state.write = 0;
            state.fill = 0;
            state.overflow_warned = false;
            state.primed = false;
            self.clears.fetch_add(1, Ordering::Relaxed);
        }
        tracing::warn!("Audio queue exceeded latency threshold, clearing queued audio");
    }

    /// Warn-Flagge (Testbeobachtung von audio_out_overflow_logged).
    #[cfg(test)]
    fn overflow_warned(&self) -> bool {
        self.state.lock().map(|s| s.overflow_warned).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Volume (Port des SDL_MixAudioFormat-Aufrufs aus PushAudioFrame)
// ---------------------------------------------------------------------------

/// C++: `SDL_MixAudioFormat(..., AUDIO_S16SYS, volume)` rechnet
/// `sample * volume / 128` (ganzzahlig); bei `volume == SDL_MIX_MAXVOLUME`
/// macht das C++ stattdessen ein reines memcpy (Identität).
/// Läuft auf i32 und kann nicht überlaufen: |i16| * 127 / 128 < i16::MAX.
fn apply_volume(samples: &mut [i16], volume128: i32) {
    if volume128 == SDL_MIX_MAXVOLUME as i32 {
        return; // C++-memcpy-Zweig
    }
    for s in samples.iter_mut() {
        *s = ((*s as i32) * volume128 / SDL_MIX_MAXVOLUME as i32) as i16;
    }
}

// ---------------------------------------------------------------------------
// AudioOutput
// ---------------------------------------------------------------------------

/// Shared State zwischen Audio-Thread (push), Drain-Thread (Stufe 1 → 2)
/// und cpal-Callback.
struct OutShared {
    /// Stufe 1: Decode-Seite — der Audio-Thread pusht sofort bei Ankunft;
    /// Überlauf → Drop-Oldest. Kein Guard (C++ `QueueAudioOutData`).
    decode_ring: SampleRing,
    /// Stufe 2: Device-Seite — der Drain-Thread hält sie auf dem C++-
    /// Drain-Target (2×buffer), der cpal-Callback zieht daraus. Der
    /// 3×-Guard lebt im Drain (C++: SDL_GetQueuedAudioSize > 3×buffer).
    device_ring: SampleRing,
    /// `settings/audio_volume` 0..=128 (SDL_MIX_MAXVOLUME-Skala).
    volume128: AtomicU32,
    /// Session-Format (das Format, das `push` liefert) — für fill_ms.
    sample_rate: u32,
    channels: u16,
    /// Drain-Thread-Steuerung (Drop des AudioOutput setzt Stop + Weckruf).
    stop: AtomicBool,
    drain_mx: Mutex<()>,
    drain_cv: std::sync::Condvar,
}

/// Audio-Ausgabe (Lautsprecher). Port von `InitAudio`/`PushAudioFrame`.
///
/// **Nicht `Send`** (cpal::Stream): Erzeugen und Droppen müssen auf demselben
/// Thread passieren — die Session baut das Objekt beim Audio-Handshake
/// (`AudioSettingsCb` → `InitAudio`) ab und wirft es beim Stopp weg.
///
/// Der Ring hat exakt die C++-Kapazität `8 * audio_buffer_size` Bytes;
/// `push` darf vom Session-/Decoder-Thread gerufen werden, solange das
/// `AudioOutput`-Objekt lebt.
pub struct AudioOutput {
    // RAII: hält den cpal-Stream am Leben (Drop stoppt den Callback).
    #[allow(dead_code)]
    stream: Stream,
    shared: Arc<OutShared>,
    device_name: String,
    /// Tatsächlich ausgehandeltes Geräteformat (C++: `obtained`).
    obtained_sample_rate: u32,
    obtained_channels: u16,
    obtained_sample_format: SampleFormat,
    /// Angeforderte Puffergröße in Frames (C++: `spec.samples`).
    requested_frames: u32,
}

impl AudioOutput {
    /// Öffnet das Ausgabegerät und startet den Stream.
    ///
    /// * `device` — Gerätename aus [`AudioOutput::devices`], `None`/`""` = Standardgerät.
    ///   Bei nicht auffindbarem Namen wird — wie im C++ (`InitAudio`) — auf das
    ///   Standardgerät zurückgefallen (Fallback-Logtext übernommen).
    /// * `sample_rate`/`channels` — Session-Format aus dem AudioHeader
    ///   (`AudioSettingsCb(channels, rate)`, immer 48 kHz stereo bei Remote Play).
    /// * `buffer_size` — roher Wert von `settings/audio_buffer_size` (**Bytes**
    ///   S16-PCM, wie im C++; `0` = [`DEFAULT_AUDIO_BUFFER_SIZE`]). Daraus folgen
    ///   Ringkapazität (×8), Latenzgrenze (×3) und die angeforderte
    ///   Geräte-Puffergröße in Frames (`buffer_size / (2 * channels)`,
    ///   C++: `spec.samples = audio_buffer_size / audio_out_sample_size`).
    pub fn new(
        device: Option<&str>,
        sample_rate: u32,
        channels: u16,
        buffer_size: u32,
    ) -> ChiakiResult<Self> {
        let host = cpal::default_host();
        let (device, device_name) = resolve_device(Direction::Output, &host, device)?;

        // C++: audio_buffer_size == 0 → GetAudioBufferSizeDefault() = 9600.
        let buffer_size = if buffer_size == 0 {
            DEFAULT_AUDIO_BUFFER_SIZE
        } else {
            buffer_size
        };
        let buffer_samples = buffer_size as usize / 2; // S16: 2 Bytes je Sample
        let requested_frames = buffer_size / (2 * u32::from(channels)); // C++ spec.samples

        let (config, sample_format, converted) =
            pick_stream_config(&device, Direction::Output, sample_rate, channels, requested_frames)?;

        let shared = Arc::new(OutShared {
            // C++: ring_buf.resize(audio_buffer_size * 8) ist in BYTES — bei
            // S16 also 38400 Samples (nicht buffer_samples × 4: das wäre nur
            // die halbe C++-Kapazität).
            decode_ring: SampleRing::new(buffer_samples * 8, 0),
            // Device-Ring: Prefill = C++-Drain-Target (2×audio_buffer_size
            // Bytes = 2×buffer_samples Samples), damit der Callback nicht
            // dauerhaft am Leerlauf-Anschlag spielt.
            device_ring: SampleRing::new(buffer_samples * 8, buffer_samples * 2),
            volume128: AtomicU32::new(SDL_MIX_MAXVOLUME),
            sample_rate,
            channels,
            stop: std::sync::atomic::AtomicBool::new(false),
            drain_mx: Mutex::new(()),
            drain_cv: std::sync::Condvar::new(),
        });

        // C++ InitAudio-Logzeile, falls SDL konvertieren musste.
        if converted {
            tracing::warn!(
                "Audio output '{}' opened with converted format {:?}, {} channels @ {} Hz (requested {:?}, {} channels @ {} Hz)",
                device_name,
                sample_format,
                config.channels,
                config.sample_rate.0,
                SampleFormat::I16,
                channels,
                sample_rate
            );
        }

        let stream = build_stream(&device, &config, sample_format, &shared)
            .or_else(|err| {
                // Fixed Buffer Size wird im WASAPI-Shared-Mode nicht von jedem
                // Treiber akzeptiert — dann der Engine-Default (wie SDL es bei
                // "obtained" auch tun durfte).
                tracing::warn!(
                    "Audio output: requested buffer size {} frames rejected ({}), retrying with engine default",
                    requested_frames,
                    err
                );
                let config = StreamConfig {
                    channels: config.channels,
                    sample_rate: config.sample_rate,
                    buffer_size: BufferSize::Default,
                };
                build_stream(&device, &config, sample_format, &shared)
            })
            .map_err(|_| ChiakiError::Unknown)?;

        // C++: SDL_PauseAudioDevice(audio_out, 0) — der Stream spielt sofort.
        stream.play().map_err(|_| ChiakiError::Unknown)?;

        // Drain-Thread (Stufe 1 → 2), C++ `AudioOutDrainThreadMain`: hält
        // den Device-Ring auf 2×buffer und wirft bei 3× den Rückstand weg.
        {
            let shared = Arc::clone(&shared);
            let chunk_cap = buffer_samples * 2; // größer als ein Drain-Nachschuss je Fall
            std::thread::Builder::new()
                .name("chiaki-media-audio-drain".into())
                .spawn(move || {
                    let mut scratch: Vec<i16> = Vec::with_capacity(chunk_cap);
                    let mx = shared.drain_mx.lock().unwrap_or_else(|e| e.into_inner());
                    let mut mx_guard = mx;
                    loop {
                        if shared.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        drop(mx_guard);
                        // Nachschub: Device-Ring auf das Drain-Target füllen.
                        loop {
                            let fill = shared.device_ring.fill() as usize;
                            if fill >= buffer_samples * 2 {
                                break;
                            }
                            let want = (buffer_samples * 2 - fill).min(chunk_cap);
                            let got = shared.decode_ring.pop(want, &mut scratch);
                            if got == 0 {
                                break;
                            }
                            // Push mit dem 3×-Guard als Sicherheitsnetz —
                            // regulär hält der Drain das Ziel ein.
                            shared.device_ring.push(&scratch[..got], u64::MAX);
                        }
                        let mx2 = shared.drain_mx.lock().unwrap_or_else(|e| e.into_inner());
                        mx_guard = mx2;
                        // Auf Nachschub warten (Push weckt; 20 ms Tick als
                        // Netz gegen verlorene Wakeups).
                        if shared.decode_ring.fill() == 0 {
                            let (g, _timeout) = shared
                                .drain_cv
                                .wait_timeout(mx_guard, std::time::Duration::from_millis(20))
                                .unwrap_or_else(|e| e.into_inner());
                            mx_guard = g;
                        }
                    }
                })
                .expect("Audio-Drain-Thread starten");
        }

        tracing::info!(
            "Audio Device '{}' opened with {} channels @ {} Hz, buffer size {}",
            device_name,
            config.channels,
            config.sample_rate.0,
            requested_frames * u32::from(config.channels) * 2 // obtained.size-Äquivalent in Bytes
        );

        Ok(AudioOutput {
            obtained_sample_rate: config.sample_rate.0,
            obtained_channels: config.channels,
            obtained_sample_format: sample_format,
            requested_frames,
            device_name,
            stream,
            shared,
        })
    }

    /// Port von `PushAudioFrame` (ohne Speex-Echo-Referenz): hängt dekodierte
    /// Samples (interleaved i16 im Session-Format) an den Ring an.
    ///
    /// C++-Semantik übernommen: bei `audio_volume == 0` wird nichts mehr
    /// eingereiht (`if(!audio_out || !audio_volume) return;`), bei Rückstand
    /// über 3× Puffergröße wird der komplette Queue-Inhalt verworfen und bei
    /// Ringüberlauf die ältesten Samples gedroppt.
    pub fn push(&self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        // C++: if(!audio_out || !audio_volume) return;
        if self.shared.volume128.load(Ordering::Relaxed) == 0 {
            return;
        }
        // Stufe 1: ohne Guard — Ankunftsbursts werden vom Decode-Ring
        // geschluckt (Drop-Oldest jenseits der Kapazität). Der 3×-Guard
        // liegt auf der Device-Seite (Drain-Thread).
        self.shared.decode_ring.push(samples, u64::MAX);
        self.shared.drain_cv.notify_all();
    }

    /// Lautstärke 0.0..=1.0 (Einstellung `settings/audio_volume` 0..=128 →
    /// `volume / 128`). Wird im Audio-Callback angewendet und ist dadurch —
    /// anders als im C++, wo beim Reihen gemischt wurde — sofort wirksam.
    /// Die Rechnung ist identisch (`sample * volume128 / 128`).
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        let volume128 = (clamped * SDL_MIX_MAXVOLUME as f32).round() as u32;
        self.shared
            .volume128
            .store(volume128.min(SDL_MIX_MAXVOLUME), Ordering::Relaxed);
    }

    /// Aktuelle Lautstärke auf der SDL-Skala 0..=128.
    pub fn volume128(&self) -> u32 {
        self.shared.volume128.load(Ordering::Relaxed)
    }

    /// Ring-Füllstand in Millisekunden Session-Audio — für das Stats-HUD
    /// (C++ hatte dafür nur ein auskommentiertes qDebug über die SDL-Queue).
    pub fn current_buffer_fill_ms(&self) -> f32 {
        // Device-Seite: das ist die echte Ausgabelatenz (C++: SDL-Queue).
        let fill = self.shared.device_ring.fill() as f32;
        let samples_per_ms = f32::from(self.shared.channels) * self.shared.sample_rate as f32
            / 1000.0;
        if samples_per_ms <= 0.0 {
            0.0
        } else {
            fill / samples_per_ms
        }
    }

    /// Durch Überlauf verworfene Samples (kumulativ).
    pub fn dropped_samples(&self) -> u64 {
        self.shared.decode_ring.dropped.load(Ordering::Relaxed)
            + self.shared.device_ring.dropped.load(Ordering::Relaxed)
    }

    /// In den Ring geschobene Samples (kumulativ) — Messung Producer-/Konsum-
    /// Bilanz (HANDOFF P1 "Audio queue exceeded").
    pub fn pushed_samples(&self) -> u64 {
        self.shared.decode_ring.pushed.load(Ordering::Relaxed)
    }

    /// Vom Gerät-Callback entnommene Samples (kumulativ).
    pub fn pulled_samples(&self) -> u64 {
        self.shared.device_ring.pulled.load(Ordering::Relaxed)
    }

    /// Ausgelöste 3×-Latenz-Clears (kumulativ).
    pub fn clears(&self) -> u64 {
        self.shared.device_ring.clears.load(Ordering::Relaxed)
    }

    /// Callbacks, in denen Stille wegen leerem Ring nachgespielt wurde.
    pub fn underflows(&self) -> u64 {
        self.shared.device_ring.underflows.load(Ordering::Relaxed)
    }

    /// Aufgelöster Gerätename (nach Fallback auf den Default).
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Tatsächliche Geräte-Parameter (C++: `obtained`-Spec).
    pub fn obtained_config(&self) -> (u32, u16, SampleFormat) {
        (
            self.obtained_sample_rate,
            self.obtained_channels,
            self.obtained_sample_format,
        )
    }

    /// Angeforderte Geräte-Puffergröße in Frames (C++: `spec.samples`).
    pub fn requested_buffer_frames(&self) -> u32 {
        self.requested_frames
    }

    /// Geräte-Liste für die Settings-UI: Standardgerät zuerst (wie die
    /// C++-UI mit "Auto"), danach alle weiteren Ausgabegeräte.
    pub fn devices() -> Vec<String> {
        super::devices(Direction::Output)
    }
}

impl std::fmt::Debug for AudioOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioOutput")
            .field("device", &self.device_name)
            .field("sample_rate", &self.obtained_sample_rate)
            .field("channels", &self.obtained_channels)
            .field("sample_format", &self.obtained_sample_format)
            .finish()
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        // Drain-Thread stoppen (Stream Drop danach im Feld-Teardown —
        // der cpal-Callback endet mit dem Stream).
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.drain_cv.notify_all();
    }
}

// Drop des Streams stoppt den cpal-Callback (C++: SDL_CloseAudioDevice).

// ---------------------------------------------------------------------------
// Geräte-/Format-Auswahl: gemeinsam in `super` (mod.rs)
// ---------------------------------------------------------------------------

/// Baut den cpal-Stream im passenden Sample-Format. Der Callback füllt aus
/// dem Ring (i16, Session-Kanäle), mischt die Lautstärke, wandelt ggf. die
/// Kanalzahl und dann das Sample-Format — die Arbeit, die im C++ SDL
/// (`SDL_MixAudioFormat` + AudioConverter) abgenommen hat.
fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    shared: &Arc<OutShared>,
) -> Result<Stream, cpal::BuildStreamError> {
    // Callback-Scratch: eine Geräte-Periode reicht; größere Callbacks werden
    // Frame-weise durchlaufen.
    let max_frames = match config.buffer_size {
        BufferSize::Fixed(frames) => frames as usize,
        BufferSize::Default => (config.sample_rate.0 / 100) as usize, // typische 10-ms-Periode
    }
    .max(config.sample_rate.0 as usize / 200); // mind. 5 ms
    let sess_ch = shared.channels as usize;
    let dev_ch = config.channels as usize;
    let dev_channels = config.channels;
    let mut scratch = vec![0i16; max_frames * sess_ch];
    let mut converted = vec![0i16; max_frames * dev_ch];
    let shared = Arc::clone(shared);

    let error_callback = move |err: cpal::StreamError| {
        tracing::error!("Audio output stream error: {err}");
    };

    macro_rules! build {
        ($t:ty, $convert:expr) => {
            device.build_output_stream(
                config,
                move |out: &mut [$t], _info| {
                    // Größere Callbacks werden Frame-weise durch den Scratch
                    // gedreht; alle Chunks sind ganze Frames (der WASAPI-Host
                    // liefert ganzzahlige Frame-Anzahlen).
                    for chunk in out.chunks_mut(converted.len()) {
                        let frames = chunk.len() / dev_ch;
                        let usable = frames * dev_ch;
                        {
                            let pcm = &mut scratch[..frames * sess_ch];
                            pcm.fill(0); // Underflow → Stille (SDL-Unterrun-Verhalten)
                            shared.device_ring.pull(pcm);
                            apply_volume(pcm, shared.volume128.load(Ordering::Relaxed) as i32);
                        }
                        map_channels(
                            &scratch,
                            shared.channels,
                            frames,
                            &mut converted[..usable],
                            dev_channels,
                            frames,
                        );
                        for (dst, &src) in chunk[..usable].iter_mut().zip(converted[..usable].iter())
                        {
                            *dst = $convert(src);
                        }
                    }
                },
                error_callback,
                None,
            )
        };
    }

    match sample_format {
        SampleFormat::I16 => build!(i16, |s: i16| s),
        SampleFormat::F32 => build!(f32, |s: i16| s as f32 / 32768.0),
        SampleFormat::U16 => build!(u16, |s: i16| (s as u16) ^ 0x8000),
        SampleFormat::U8 => build!(u8, |s: i16| ((s >> 8) as i32 + 128) as u8),
        other => Err(cpal::BuildStreamError::StreamConfigNotSupported)
            .inspect_err(|_| tracing::error!("Unsupported output sample format: {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Tests (ohne Hardware)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(capacity: usize) -> SampleRing {
        SampleRing::new(capacity, 0)
    }

    #[test]
    fn ring_push_pull_roundtrip_across_wraparound() {
        let r = ring(8);
        r.push(&[1, 2, 3, 4, 5, 6], 1000);
        let mut out = [0i16; 4];
        assert_eq!(r.pull(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);

        // Über die Kapazitätsgrenze hinweg weiterschreiben …
        r.push(&[7, 8, 9, 10, 11, 12], 1000);
        let mut out = [0i16; 8];
        assert_eq!(r.pull(&mut out), 8);
        assert_eq!(out, [5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(r.fill(), 0);
    }

    #[test]
    fn ring_pull_leaves_tail_untouched_on_underflow() {
        let r = ring(8);
        r.push(&[1, 2, 3], 1000);
        let mut out = [0x55u16 as i16; 8]; // Marker
        assert_eq!(r.pull(&mut out), 3);
        assert_eq!(&out[..3], &[1, 2, 3]);
        assert_eq!(&out[3..], &[0x55; 5]);
        assert_eq!(r.underflows.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ring_overflow_drops_oldest_and_warns_once() {
        let r = ring(8);
        r.push(&[1, 2, 3, 4, 5, 6, 7, 8], 1000);
        // Volle Kapazität + 4 neue Samples → wie QueueAudioOutData: die
        // ältesten 4 (read_pos += bytes_to_drop) fallen raus.
        r.push(&[9, 10, 11, 12], 1000);
        assert_eq!(r.dropped.load(Ordering::Relaxed), 4);
        assert!(r.overflow_warned());
        let mut out = [0i16; 8];
        assert_eq!(r.pull(&mut out), 8);
        assert_eq!(out, [5, 6, 7, 8, 9, 10, 11, 12]);

        // Warnung nur einmal (audio_out_overflow_logged), Reset nach Leerlauf.
        r.push(&[13, 14, 15, 16], 1000);
        let mut out = [0i16; 4];
        r.pull(&mut out);
        assert!(!r.overflow_warned());
    }

    #[test]
    fn ring_data_larger_than_capacity_keeps_tail_and_resets() {
        let r = ring(4);
        r.push(&[1, 2, 3, 4, 5, 6], 1000);
        let mut out = [0i16; 4];
        assert_eq!(r.pull(&mut out), 4);
        assert_eq!(out, [3, 4, 5, 6]);
    }

    #[test]
    fn ring_clear_threshold_drops_backlog_like_cpp_queue_clear() {
        let r = ring(16);
        // Threshold 8 Samples: fill > 8 löst den C++-Queue-Clear aus.
        r.push(&[1, 2, 3, 4], 8);
        r.push(&[5, 6, 7, 8], 8); // fill=4 → ok
        r.push(&[9, 10, 11, 12], 8); // fill=8, 8 > 8 ist false → noch kein Clear
        let mut out = [0i16; 16];
        assert_eq!(r.pull(&mut out), 12);
        assert_eq!(&out[..12], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

        // Rückstand über dem Threshold → beim nächsten Push fliegt der
        // komplette Stau raus, nur die neuen Frames bleiben.
        r.push(&[1, 2, 3, 4], 8);
        r.push(&[5, 6, 7, 8], 8);
        r.push(&[9, 10, 11, 12], 8); // fill=12
        r.push(&[21, 22, 23, 24], 8); // fill=12 > 8 → Clear, dann push
        r.push(&[31, 32, 33, 34], 8); // fill=4 → ok
        let mut out2 = [0i16; 16];
        let got = r.pull(&mut out2);
        assert_eq!(got, 8);
        assert_eq!(&out2[..got], &[21, 22, 23, 24, 31, 32, 33, 34]);
    }

    #[test]
    fn volume_matches_sdl_mix_audio_format() {
        // SDL_MixAudioFormat: sample * volume / 128 (ganzzahlig)
        let mut s = vec![1000i16, -2000, i16::MAX, i16::MIN];
        apply_volume(&mut s, 128); // Identität (C++-memcpy-Zweig)
        assert_eq!(s, vec![1000, -2000, i16::MAX, i16::MIN]);

        let mut s = vec![1000i16, -2000];
        apply_volume(&mut s, 64);
        assert_eq!(s, vec![500, -1000]);

        let mut s = vec![1234i16];
        apply_volume(&mut s, 0);
        assert_eq!(s, vec![0]);

        // Lautstärken unter 128 können i16 nicht überlaufen lassen
        // (ganzzahlig wie SDL: 32767*127/128 = 32511).
        let mut s = vec![i16::MIN, i16::MAX];
        apply_volume(&mut s, 127);
        assert_eq!(s, vec![-32512, 32511]);
    }

    #[test]
    fn ring_is_quiet_when_empty_and_flags_reset() {
        let r = ring(4);
        let mut out = [0i16; 4];
        assert_eq!(r.pull(&mut out), 0);
        assert_eq!(r.underflows.load(Ordering::Relaxed), 1);
        r.push(&[1, 2], 1000);
        r.push(&[3, 4, 5], 1000); // 3 > 4-2 → das älteste Sample fliegt raus
        assert!(r.overflow_warned());
        let mut four = [0i16; 4];
        assert_eq!(r.pull(&mut four), 4);
        assert_eq!(four, [2, 3, 4, 5]);
        // Leerlauf setzt die Warn-Flagge zurück (C++-Verhalten).
        assert!(!r.overflow_warned());
    }

    /// Prime-Phase: Mit Prefill spielt der Callback erst Stille, bis die
    /// Vorfüllung steht; danach normal. Zähler: die Prime-Stille zählt
    /// NICHT als Underflow — nur Stille bei laufendem Spiel (primed).
    #[test]
    fn ring_prefills_before_playing_and_reprimes_after_underrun() {
        let r = SampleRing::new(16, 8);
        let mut out = [0i16; 4];

        // Unter der Vorfüllung: Stille, kein Underflow gezählt.
        r.push(&[1, 2, 3, 4], 1000);
        assert_eq!(r.pull(&mut out), 0);
        assert_eq!(r.underflows.load(Ordering::Relaxed), 0);

        // Vorfüllung erreicht (4 + 4 weitere = 8) → spielt alles raus.
        r.push(&[5, 6, 7, 8], 1000);
        assert_eq!(r.pull(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);
        assert_eq!(r.pull(&mut out), 4);
        assert_eq!(out, [5, 6, 7, 8]);
        // Leer gespielt → neu geprimed (Stille bis wieder 8 da sind).
        assert_eq!(r.pull(&mut out), 0);
        assert_eq!(r.underflows.load(Ordering::Relaxed), 0);
        r.push(&[9, 10, 11, 12], 1000);
        assert_eq!(r.pull(&mut out), 0, "unter Prefill wieder still");
        r.push(&[13, 14, 15, 16], 1000);
        assert_eq!(r.pull(&mut out), 4);
        assert_eq!(out, [9, 10, 11, 12]);
        assert_eq!(r.pull(&mut out), 4);
        assert_eq!(out, [13, 14, 15, 16]);
        assert_eq!(r.underflows.load(Ordering::Relaxed), 0);

        // Echter Underflow IM Spiel: primed, aber der Ring hält weniger,
        // als der Callback haben will → Stille-Rest wird gezählt.
        r.push(&[17, 18, 19, 20], 1000);
        r.push(&[21, 22, 23, 24], 1000); // fill=8 → primed
        let mut big = [0i16; 16];
        assert_eq!(r.pull(&mut big), 8);
        assert_eq!(&big[..8], &[17, 18, 19, 20, 21, 22, 23, 24]);
        assert_eq!(r.underflows.load(Ordering::Relaxed), 1);
        // fill=0 → neu geprimed: weitere Stille zählt nicht mehr.
        assert_eq!(r.pull(&mut big), 0);
        assert_eq!(r.underflows.load(Ordering::Relaxed), 1);
    }

    /// Clear während der Prime-Phase: Fill-Reset → primed wird zurück-
    /// gesetzt und es wird neu vorgefüllt, bevor wieder gespielt wird.
    #[test]
    fn ring_clear_resets_prime_state() {
        let r = SampleRing::new(16, 8);
        let mut out = [0i16; 2];
        // fill=8 ≥ prefill → primed; dann 2 ziehen (fill=6).
        r.push(&[1, 2, 3, 4], 4); // fill 4 > 4 ist false → kein Clear
        r.push(&[5, 6, 7, 8], 4); // fill 8, kein Clear (8 > 4 false)
        r.pull(&mut out);
        // fill=10 > threshold 4 → CLEAR (fill=0, primed=false), dann push.
        r.push(&[9, 10, 11, 12], 4);
        assert_eq!(r.pull(&mut out), 0, "nach Clear wird neu geprimed");
        r.push(&[13, 14, 15, 16], 4); // fill=8 ≥ 8 → primed
        assert_eq!(r.pull(&mut out), 2);
        assert_eq!(out, [9, 10]);
    }

    /// Stream-Test gegen das Standardgerät: Aufbau, 100 ms Audio in den
    /// Ring, der cpal-Callback muss es abholen; danach sauberer Stop.
    /// `#[ignore]`, damit CI ohne Audio-Gerät grün bleibt (diese Maschine
    /// HAT Audio: lokal mit `--ignored` laufen lassen).
    #[test]
    #[ignore = "benoetigt ein Ausgabegeraet (Windows-Host)"]
    fn output_stream_plays_100ms_silence_and_stops_cleanly() {
        let out = AudioOutput::new(None, 48_000, 2, 0).expect("AudioOutput auf Standardgerät");
        out.set_volume(1.0);
        // 100 ms @ 48 kHz stereo = 4800 Frames = 9600 Samples (Stille).
        let silence = vec![0i16; 9600];
        out.push(&silence);
        // Zweistufig: push landet im Decode-Ring, der Drain-Thread überführt
        // in den Device-Ring — nach 300 ms hat der Callback beides verbraucht
        // (Underflow zählt ggf. die Restperiode — Hauptsache, nichts ist mehr
        // angestaut).
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(out.current_buffer_fill_ms() < 1.0);
        println!(
            "underflows: {}, dropped: {}",
            out.underflows(),
            out.dropped_samples()
        );
        drop(out); // sauberer Stop (C++: SDL_CloseAudioDevice)
    }

    /// Zwei-Stufen-Durchlauf (Kern des Drain-Threads): Decode-Ring → pop →
    /// Device-Ring → pull liefert die Daten in Original-Reihenfolge; Bursts
    /// im Decode-Ring ändern das Ergebnis nicht.
    #[test]
    fn two_stage_drain_preserves_order() {
        let decode = ring(64);
        let device = SampleRing::new(64, 0);
        let mut scratch: Vec<i16> = Vec::new();

        // Ankunftsburst: alles auf einmal in Stufe 1.
        decode.push(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], u64::MAX);

        // Drain-Schritt wie im Drain-Thread (pop → push, ohne Guard).
        let got = decode.pop(4, &mut scratch);
        assert_eq!(got, 4);
        device.push(&scratch, u64::MAX);

        let mut out = [0i16; 4];
        assert_eq!(device.pull(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);

        // Rest rüberdrainen, inklusive Wrap-around-Kanten.
        let got = decode.pop(8, &mut scratch);
        assert_eq!(got, 6);
        device.push(&scratch, u64::MAX);
        let mut out = [0i16; 8];
        assert_eq!(device.pull(&mut out), 6);
        assert_eq!(&out[..6], &[5, 6, 7, 8, 9, 10]);
        assert_eq!(decode.fill(), 0);
    }

    /// Geräte-Enumeration gegen den WASAPI-Host.
    #[test]
    #[ignore = "benoetigt Audio-Geraete (Windows-Host)"]
    fn enumerates_output_devices() {
        let devices = AudioOutput::devices();
        assert!(!devices.is_empty(), "kein Ausgabegeraet gefunden");
        println!("output devices: {devices:?}");
    }
}
