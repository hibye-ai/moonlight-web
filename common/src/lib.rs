use log::warn;
use moonlight_common::stream::video::VideoFormats;
use serde::Serialize;

use crate::api_bindings::{StreamPermissions, StreamSettings};

pub mod api_bindings;
pub mod api_bindings_ext;
pub mod config;
pub mod ipc;

pub const MAX_STREAM_BITRATE_KBPS: u32 = 500_000;
pub const MAX_STREAM_FPS: u32 = 240;
pub const MIN_STREAM_DIMENSION: u32 = 64;
pub const MAX_STREAM_DIMENSION: u32 = 7_680;
pub const MAX_STREAM_PIXELS: u64 = 7_680 * 4_320;

/// Applies hard safety limits to untrusted client-provided stream settings.
/// Role-specific restrictions are applied separately afterwards.
pub fn apply_absolute_stream_limits(settings: &mut StreamSettings) {
    settings.bitrate_kbps = settings.bitrate_kbps.clamp(1, MAX_STREAM_BITRATE_KBPS);
    settings.fps = settings.fps.clamp(1, MAX_STREAM_FPS);
    settings.width = settings
        .width
        .clamp(MIN_STREAM_DIMENSION, MAX_STREAM_DIMENSION);
    settings.height = settings
        .height
        .clamp(MIN_STREAM_DIMENSION, MAX_STREAM_DIMENSION);

    let pixels = u64::from(settings.width) * u64::from(settings.height);
    if pixels > MAX_STREAM_PIXELS {
        let scale = (MAX_STREAM_PIXELS as f64 / pixels as f64).sqrt();
        settings.width = ((settings.width as f64 * scale).floor() as u32)
            .clamp(MIN_STREAM_DIMENSION, MAX_STREAM_DIMENSION);
        settings.height = ((settings.height as f64 * scale).floor() as u32)
            .clamp(MIN_STREAM_DIMENSION, MAX_STREAM_DIMENSION);
    }
}

/// Applies the permissions / restrictions to the current settings of the user.
/// This won't error, it'll just overwrite it, because the GUI should indicate those restrictions.
pub fn apply_permissions_to_settings(
    permissions: &StreamPermissions,
    settings: &mut StreamSettings,
) {
    apply_absolute_stream_limits(settings);
    let StreamPermissions {
        allow_add_hosts: _,
        maximum_bitrate_kbps,
        allow_codec_h264,
        allow_codec_h265,
        allow_codec_av1,
        allow_hdr,
        allow_transport_webrtc: _,
        allow_transport_websockets: _,
    } = permissions;

    if let Some(maximum_bitrate) = maximum_bitrate_kbps
        && settings.bitrate_kbps > *maximum_bitrate
    {
        settings.bitrate_kbps = *maximum_bitrate;
    }

    let mut supported_codecs = VideoFormats::from_bits_truncate(settings.supported_codecs);
    if !allow_codec_h264 {
        supported_codecs &= !VideoFormats::MASK_H264;
    }
    if !allow_codec_h265 {
        supported_codecs &= !VideoFormats::MASK_H265;
    }
    if !allow_codec_av1 {
        supported_codecs &= !VideoFormats::MASK_AV1;
    }
    settings.supported_codecs = supported_codecs.bits();

    if !allow_hdr {
        settings.hdr = false;
    }

    // Transport restrictions are handled in the streamer
}

pub fn serialize_json<T>(message: &T) -> Option<String>
where
    T: Serialize,
{
    let Ok(json) = serde_json::to_string(&message) else {
        warn!("[Stream]: failed to serialize to json");
        return None;
    };

    Some(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_stream_limits_clamp_untrusted_values() {
        let mut settings = StreamSettings {
            bitrate_kbps: u32::MAX,
            width: u32::MAX,
            height: u32::MAX,
            fps: u32::MAX,
            play_audio_local: false,
            supported_codecs: u32::MAX,
            hdr: false,
        };

        apply_absolute_stream_limits(&mut settings);

        assert_eq!(settings.bitrate_kbps, MAX_STREAM_BITRATE_KBPS);
        assert_eq!(settings.fps, MAX_STREAM_FPS);
        assert!(settings.width >= MIN_STREAM_DIMENSION);
        assert!(settings.height >= MIN_STREAM_DIMENSION);
        assert!(settings.width <= MAX_STREAM_DIMENSION);
        assert!(settings.height <= MAX_STREAM_DIMENSION);
        assert!(u64::from(settings.width) * u64::from(settings.height) <= MAX_STREAM_PIXELS);
    }

    #[test]
    fn absolute_stream_limits_preserve_normal_values() {
        let mut settings = StreamSettings {
            bitrate_kbps: 50_000,
            width: 3_840,
            height: 2_160,
            fps: 120,
            play_audio_local: false,
            supported_codecs: 0,
            hdr: true,
        };

        apply_absolute_stream_limits(&mut settings);

        assert_eq!(settings.bitrate_kbps, 50_000);
        assert_eq!(settings.width, 3_840);
        assert_eq!(settings.height, 2_160);
        assert_eq!(settings.fps, 120);
    }
}
