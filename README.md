# chiaki-rust

A complete from-scratch Rust port of the [chiaki-ng](https://murilex07.github.io) PS5/PS4 remote-play client for **Windows x64**, with a native GPU UI, a fully GPU-resident video path, NVIDIA VSR upscaling, and an OBS virtual camera feed with a headless mode.

![Home](docs/screenshots/home.png)

> **Work in progress:** I develop this in my spare time and keep shipping improvements — expect regular updates and the occasional rough edge.

**Measured in live LAN sessions against a real PS5** (RTX 4090, 1080p60 H.265, ~24 Mbit/s at motion):

| Metric | Value |
|---|---|
| Round-trip time | ~1.1 ms (loss 0.0%) |
| Frame rate | stable 60 fps with VSR 2x active (1080p → 4K), 0 dropped frames over multi-minute sessions |
| Media pipeline | ~2.2 ms per frame vs. 16.7 ms budget (decode 0.85 ms + VSR 1.32 ms) — fully GPU-resident, zero CPU frame copies |
| Audio | Opus out/in, ~28 ms buffer (matches the C++ client's semantics) |
| Decoder backends | NVDEC-CUDA, D3D11VA, Vulkan, software (auto or per setting) |
| Test suite | 649 automated tests, including golden vectors verified byte-identical against the compiled C code |

## Stream to Twitch or Discord — no capture card needed

The classic console streaming setup needs a capture card. This client takes a different route: the PS5 stream is fed directly into the **OBS Virtual Camera**, so

- **OBS** binds it as a plain video source and your Twitch/Kick/YouTube scene gets the gameplay — no capture card, no extra hardware.
- **Discord** binds it as a regular webcam: show your game in a voice call while chatting.
- With **NVIDIA VSR** enabled, viewers don't just see the stream — they see it upscaled to up to 4K, sharper than the PS5's own output.

Handy for big console releases (GTA VI on PS5, for example): play on the couch, stream from the PC, nobody needs a capture card. Details in the virtual camera section below.

## Why this fork of reality exists

This is a 1:1 port: the C++ code is treated as the spec — protocol state machines, constants, timeouts and byte formats are carried over exactly, while memory management is idiomatic safe Rust (`#![deny(unsafe_code)]` in the protocol crates; FFI confined to the media/render/input layers). Settings INI files are **byte-compatible with the C++ client** (Qt QSettings format), so existing users can switch binaries without re-registering.

Everything runs: discovery, registration wizard, LAN streaming (H.264/H.265, 720p–1080p @30/60), PSN remote play over the internet (holepunch/RUDP/UPnP/STUN), DualSense with rumble/adaptive triggers/haptics, microphone, sleep/wake, PIN login, PSN OAuth — plus the two features below that go beyond the C++ client.

## Requirements

- **Windows 10/11 x64** and any DirectX 11 GPU. Hardware decode picks D3D11VA (AMD/Intel/NVIDIA), CUDA (NVIDIA), Vulkan or software, automatically or per setting.
- Nothing else is required to run the portable release — FFmpeg and Opus DLLs are bundled. **VSR and the virtual camera have extra needs** (see their sections): VSR wants an NVIDIA RTX GPU plus NVIDIA's Video Effects SDK (not redistributable, so not bundled), the virtual camera wants OBS Studio installed once.

## NVIDIA VSR (RTX Video Super Resolution)

The video path keeps frames GPU-resident end to end: NVDEC CUDA decode → VSR inference → CUDA↔D3D11 interop → swapchain present, with a transparent GPUI overlay window for HUD and dialogs. No per-frame CPU round-trip. The measured media pipeline cost is ~2.2 ms/frame at 1080p→4K VSR 2x on an RTX 4090 (budget: 16.7 ms).

VSR upscales the stream to up to 4K in real time (auto-targeting the display resolution, like browser VSR). The active upscale factor is shown as a badge in the stream, and per-frame stats (bitrate, RTT, loss, frame time, jitter) live in the HUD.

**Setup**

1. You need a GeForce RTX GPU (VSR is RTX-only) with current drivers. The driver alone is **not** enough — NVIDIA's VSR runtime for third-party apps ships in the Video Effects SDK, not in the GeForce driver package.
2. Download the **NVIDIA Video Effects SDK** from the [RTX Video SDK page](https://murilex07.github.io) (a free NVIDIA account may be required) and unpack it somewhere.
3. Point the client at it — any of these works (checked in this order):
   - `Settings → Video → VFX SDK path` (folder containing `NVVideoEffects.dll`)
   - the `CHIAKI_VSR_SDK_DIR` environment variable
   - `vfx_sdk/sdk/VideoFX/bin` next to (or one level above) `chiaki.exe` — the portable-zip layout
   - `C:\Program Files\NVIDIA Corporation\VFXSDK\VideoFX\bin`

`Settings → Video` shows a live status line (driver found / SDK found / ready), and every session re-checks availability before starting: if the driver or the SDK is missing, the client logs the reason, shows a toast, and **streams without upscaling instead of failing** — VSR never takes the video down with it. Without VSR, decode runs on D3D11VA (or CUDA/Vulkan/software, selectable) on the same GPU-resident path.

## Virtual camera — stream into Discord/OBS as a webcam

The stream content is fed into the **OBS Virtual Camera**, so Discord, OBS, or any DirectShow consumer can bind it as a webcam — with VSR active, the camera receives the **full VSR-upscaled output** (e.g. 1080p stream → 4K camera), so viewers get the sharp picture.

- Enable it under `Settings → Video → Virtuelle Kamera` (camera resolution selectable without VSR).
- Requires OBS Studio to be installed once (its DirectShow filter is what apps see); OBS itself does **not** need to run — but must not start its own Virtual Camera at the same time (one writer).
- Restart Discord after the first session so it lists the camera.

**Headless mode:** `chiaki.exe --virtualcam [host|IP]` runs the whole feed without any window — session, decode, VSR, camera, plus audio playback on the local PC (you hear the game; the windowless process feeds Discord/OBS). From the GUI you can start/stop this detached feed (`Settings → Video` shows its status; with the camera setting enabled, clicking a console tile offers *normal stream* vs. *headless start/stop*), and an autostart toggle registers it for the next Windows login. Only one instance runs at a time (instance mutex + named stop event, robust even after a hard kill). Verified live: 1080p stream → 3840×2160@60 camera feed with 0 feed errors.

## Architecture

| Crate | Contents |
|---|---|
| `chiaki-core` | Complete protocol: takion, ctrl (TCP + RUDP), session/streamconnection state machines, regist, discovery, senkusha, FEC (byte-identical jerasure port), crypto (rpcrypt/gkcrypt/ECDH), protobuf, golden-value tests |
| `chiaki-remote` | holepunch (6065 LOC C, 1:1), rudp, stun, PSN auth/token refresh |
| `chiaki-media` | FFmpeg loaded dynamically (NVDEC-CUDA/D3D11VA/Vulkan + software), NVIDIA VSR FFI, Opus, WASAPI audio in/out, D3D11 staging downloads |
| `chiaki-render` | D3D11 video sink + NV12/RGBA shaders, CUDA↔D3D11 interop, CPU presenter fallback |
| `chiaki-input` | gilrs (XInput), native DualSense/DS4 via hidapi (rumble/triggers/haptics/LED), ViGEm virtual pads, keyboard mapping |
| `chiaki-settings` | Qt-QSettings byte-compatible INI, hosts/PSN/profiles |
| `chiaki-virtualcam` | OBS Virtual Camera feed (NV12, stride destriping, downscale), headless IPC (instance mutex, stop event, PID file) |
| `chiaki-steam` | Steam library shortcuts (VDF, grid art, controller layout) |
| `chiaki-ui` | GPUI app: home, consoles, registration wizard, settings (8 categories, ~190 rows, live search), stream view with HUD, PSN login |
| `chiaki-app` / `chiaki-cli` | The `chiaki.exe` binary (incl. headless virtualcam mode) and a headless test CLI (`discover`/`regist`/`stream`/`wake`) |

**Video path (GPU, default):** takion recv → frame processor (FEC) → media thread: NVDEC CUDA decode (raw device frames) → VSR (`process_frame_gpu`) → CUDA→D3D11 interop write into the sink texture → passthrough shader + letterbox → present. Without VSR: D3D11VA decode → GPU-internal copy. CPU fallback path available; frame pacing and vsync are optional per setting.

## Releases and building

### Download

Releases carry a **portable zip** (`chiaki-rust-win64-portable-<version>.zip`, built automatically by GitHub Actions on every version tag): unpack anywhere, run `chiaki.exe`, keep the `data/` folder next to it for settings. FFmpeg and Opus DLLs are bundled; the NVIDIA VFX SDK is not (license) — VSR users unpack it themselves (above).

### From source

Windows x64 only. Toolchain is pinned in `rust-toolchain.toml`.

```
cargo build --release
cargo test --workspace
```

The app loads FFmpeg (avutil-59, avcodec-61, swscale-8, swresample-5 — any FFmpeg 7.1 win64 *shared* build) and Opus (`opus.dll`/`libopus-0.dll`) next to the exe at runtime. `scripts/build-portable-zip.ps1 -SmokeTest` assembles the portable layout locally.

Tips:

- `CHIAKI_UI_FAKE_STREAM=1 ./target/release/chiaki.exe` renders a synthetic test stream — full UI/HUD/camera pipeline without a console.
- `cargo run -p chiaki-cli -- discover --timeout-ms 1500 --broadcast-addr <subnet>.255` (multi-NIC hosts need the interface-targeted broadcast).
- Logs land in `data/log/chiaki-ui.log` next to the exe.

## Status

Feature parity with the C++ client is complete for the Windows desktop flow (see the architecture table above; libplacebo rendering and Speex mic processing are the deliberate exceptions, replaced by the GPU shader path and omitting an optional build flag). What you will not find here is Android/Linux/console ports — Windows only, by design.

## License

AGPL-3.0-only, see [LICENSE](LICENSE) — same as chiaki-ng upstream. This project is a port of and owes everything to [chiaki-ng](https://murilex07.github.io) and the original [chiaki](https://murilex07.github.io); upstream's protocol documentation and code made it possible.

**Disclaimer:** This project is not affiliated with Sony Interactive Entertainment. PlayStation, PS4 and PS5 are trademarks of Sony Interactive Entertainment Inc. You must own a console and a legitimate account; this client does not bypass any authentication.
