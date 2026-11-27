//! Debugging aid: `VICTORY_AUDIO_CAPTURE=<file.wav>` records what the title
//! plays, 48 kHz stereo 16-bit, through process loopback: our own process
//! tree only, never the rest of the host's mix. Stretches where nothing was
//! played are written as silence, so the file keeps real time.
//!
//! The recorder runs in a child copy of this executable: an audio client
//! in the title's process raises that process's timer resolution, which
//! changes how the title's own waits behave.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, Sleep, WaitForSingleObject,
};
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

const RATE: u32 = 48_000;
const FRAME_BYTES: usize = 4;
// The classic RIFF size includes 36 header bytes in addition to the PCM data.
const MAX_WAV_FRAMES: u64 = (u32::MAX as u64 - 36) / FRAME_BYTES as u64;

/// Activation completes asynchronously; the recording thread polls for it.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated;

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Names the process to record, in the recorder's environment.
const TARGET: &str = "VICTORY_AUDIO_CAPTURE_OF";

/// Starts the recorder if `VICTORY_AUDIO_CAPTURE` names a file. With this
/// module's debug log, also logs once a second what a `Sleep(1)` takes in
/// this process (the title's millisecond waits get the same).
pub fn start() {
    if tracing::enabled!(tracing::Level::DEBUG) {
        let _ = std::thread::Builder::new()
            .name("victory timing probe".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let at = Instant::now();
                    // SAFETY: plain wait.
                    unsafe { Sleep(1) };
                    let ms = at.elapsed().as_secs_f32() * 1000.0;
                    tracing::debug!(ms, "Sleep(1) took");
                }
            });
    }
    if std::env::var_os("VICTORY_AUDIO_CAPTURE").is_some()
        && let Err(e) = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .env(TARGET, std::process::id().to_string())
                .spawn()
        })
    {
        tracing::warn!("audio capture not started: {e}");
    }
}

/// In the recorder (a child started by [`start`]): records until the
/// target exits and returns true; false in any other process.
pub fn child() -> bool {
    let (Some(pid), Some(path)) = (
        std::env::var(TARGET).ok().and_then(|p| p.parse().ok()),
        std::env::var_os("VICTORY_AUDIO_CAPTURE"),
    ) else {
        return false;
    };
    if let Err(e) = record(pid, Path::new(&path)) {
        tracing::warn!("audio capture stopped: {e}");
    }
    true
}

/// Ignore gaps of up to 2 ms as before. Check the gap and the whole packet together:
/// writing either one first could leave more PCM than the header can describe.
fn packet_start(written: u64, at: u64, frames: u32) -> Option<u64> {
    let start = if at.saturating_sub(written) > u64::from(RATE / 500) {
        at
    } else {
        written
    };
    (start.checked_add(u64::from(frames))? <= MAX_WAV_FRAMES).then_some(start)
}

fn header(frames: u64) -> std::io::Result<[u8; 44]> {
    if frames > MAX_WAV_FRAMES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "too many PCM frames for a RIFF/WAVE file",
        ));
    }
    let data = (frames * FRAME_BYTES as u64) as u32;
    let mut h = [0u8; 44];
    let fields: [(&[u8], usize); 11] = [
        (b"RIFF", 0),
        (&(36 + data).to_le_bytes(), 4),
        (b"WAVEfmt ", 8),
        (&16u32.to_le_bytes(), 16),
        (&1u16.to_le_bytes(), 20),
        (&2u16.to_le_bytes(), 22),
        (&RATE.to_le_bytes(), 24),
        (&(RATE * FRAME_BYTES as u32).to_le_bytes(), 28),
        (&(FRAME_BYTES as u16).to_le_bytes(), 32),
        (&16u16.to_le_bytes(), 34),
        (b"data", 36),
    ];
    for (bytes, at) in fields {
        h[at..at + bytes.len()].copy_from_slice(bytes);
    }
    h[40..].copy_from_slice(&data.to_le_bytes());
    Ok(h)
}

fn record(pid: u32, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: COM calls with parameters built here; buffers are used only
    // between `GetBuffer` and `ReleaseBuffer`.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let target = OpenProcess(PROCESS_SYNCHRONIZE, false, pid)?;
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        /// A `PROPVARIANT` holding a blob (`VT_BLOB`).
        #[repr(C)]
        struct Blob {
            vt: u16,
            reserved: [u16; 3],
            size: u32,
            data: *const AUDIOCLIENT_ACTIVATION_PARAMS,
        }
        let blob = Blob {
            vt: 0x41,
            reserved: [0; 3],
            size: size_of_val(&params) as u32,
            data: &params,
        };
        let handler: IActivateAudioInterfaceCompletionHandler = Activated.into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some((&raw const blob).cast()),
            &handler,
        )?;
        let client = loop {
            let (mut hr, mut client) = (HRESULT(0), None::<IUnknown>);
            // Fails (`E_ILLEGAL_METHOD_CALL`) until activation completes.
            if operation.GetActivateResult(&mut hr, &mut client).is_ok() {
                hr.ok()?;
                break client.ok_or("no audio client")?.cast::<IAudioClient>()?;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * FRAME_BYTES as u32,
            nBlockAlign: FRAME_BYTES as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            10_000_000,
            0,
            &format,
            None,
        )?;
        let capture: IAudioCaptureClient = client.GetService()?;
        let mut file = File::create(path)?;
        file.write_all(&header(0)?)?;
        client.Start()?;
        tracing::info!(path = %path.display(), "recording the title's audio");

        let (mut written, mut first) = (0u64, None);
        let mut patched = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(10));
            let mut ended = WaitForSingleObject(target, 0) == WAIT_OBJECT_0;
            while capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0, 0, 0);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                // Where this packet belongs, from its time stamp (100 ns).
                let at = (qpc - *first.get_or_insert(qpc)) * u64::from(RATE) / 10_000_000;
                let Some(at) = packet_start(written, at, frames) else {
                    capture.ReleaseBuffer(frames)?;
                    tracing::warn!("audio capture stopped at the RIFF/WAVE size limit");
                    ended = true;
                    break;
                };
                if at > written {
                    tracing::debug!(
                        ms = (at - written) * 1000 / u64::from(RATE),
                        "audio capture: nothing played"
                    );
                    let silence = vec![0u8; (at - written) as usize * FRAME_BYTES];
                    file.write_all(&silence)?;
                    written = at;
                }
                let bytes = frames as usize * FRAME_BYTES;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    file.write_all(&vec![0u8; bytes])?;
                } else {
                    file.write_all(std::slice::from_raw_parts(data, bytes))?;
                }
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && written > 0 {
                    tracing::debug!("audio capture: discontinuity");
                }
                written += u64::from(frames);
                capture.ReleaseBuffer(frames)?;
            }
            if ended || patched.elapsed() >= Duration::from_secs(1) {
                // Valid sizes at any time: runs end by being killed.
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&header(written)?)?;
                file.seek(SeekFrom::End(0))?;
                patched = Instant::now();
            }
            if ended {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod wav_tests {
    use super::*;

    #[test]
    fn riff_size_and_data_size_agree_at_the_last_whole_frame() {
        let h = header(MAX_WAV_FRAMES).unwrap();
        assert_eq!(&h[0..4], b"RIFF");
        let riff = u32::from_le_bytes(h[4..8].try_into().unwrap());
        let data = u32::from_le_bytes(h[40..44].try_into().unwrap());
        assert_eq!(u64::from(data), MAX_WAV_FRAMES * FRAME_BYTES as u64);
        assert_eq!(riff, data + 36);
        assert!(u32::MAX - riff < FRAME_BYTES as u32);
        assert!(header(MAX_WAV_FRAMES + 1).is_err());
    }

    #[test]
    fn last_packet_fits_only_if_its_silence_and_pcm_both_fit() {
        let written = MAX_WAV_FRAMES - 300;
        assert_eq!(packet_start(written, written + 96, 300), Some(written));
        assert_eq!(packet_start(written, written - 1, 301), None);
        assert_eq!(
            packet_start(written, written + 100, 200),
            Some(written + 100)
        );
        assert_eq!(packet_start(written, written + 100, 201), None);
    }
}
