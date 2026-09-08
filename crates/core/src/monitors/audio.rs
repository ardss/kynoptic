//! 音频状态监控（音量 + 静音）
//!
//! 使用 winmm.dll 的 waveOutGetVolume 获取精确音量，
//! 通过注册表读取静音状态（注册表的 Mute 键比 Volume 键更可靠）。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Registry::*;

pub struct AudioMonitor {
    last_volume: Cell<u32>,
    last_muted: Cell<bool>,
}

impl Default for AudioMonitor {
    fn default() -> Self {
        Self {
            last_volume: Cell::new(u32::MAX), // 标记未初始化
            last_muted: Cell::new(false),
        }
    }
}

impl Monitor for AudioMonitor {
    fn name(&self) -> &str {
        "audio"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let (volume, muted) = read_audio_state();

        let prev_volume = self.last_volume.get();
        let prev_muted = self.last_muted.get();

        // 首次初始化，只记录并发送状态事件
        if prev_volume == u32::MAX {
            self.last_volume.set(volume);
            self.last_muted.set(muted);
            let event = Event::new(EventAction::AudioState, EventType::System)
                .data(json!({"volume": volume, "muted": muted}));
            let _ = tx.try_send(event);
            return;
        }

        // 检测音量变化
        if volume != prev_volume {
            let event = Event::new(EventAction::VolumeChange, EventType::System).data(json!({
                "old_volume": prev_volume,
                "new_volume": volume,
                "muted": muted,
            }));
            let _ = tx.try_send(event);
        }

        // 检测静音切换
        if muted != prev_muted {
            let event = Event::new(EventAction::AudioState, EventType::System).data(json!({
                "muted": muted,
                "volume": volume,
            }));
            let _ = tx.try_send(event);
        }

        self.last_volume.set(volume);
        self.last_muted.set(muted);
    }
}

/// 通过 waveOutGetVolume (winmm.dll) 获取音量，
/// 通过注册表获取静音状态。
fn read_audio_state() -> (u32, bool) {
    let volume = unsafe {
        let mut vol: u32 = 0;
        // waveOutGetVolume: 0 表示默认设备
        // 返回值: 低16位=左声道, 高16位=右声道 (0x0000-0xFFFF)
        if waveOutGetVolume(0, &mut vol) == 0 {
            let left = vol & 0xFFFF;
            let right = (vol >> 16) & 0xFFFF;
            let avg = (left + right) as f64 / 2.0;
            // 映射到 0-100
            (avg / 655.35).round() as u32
        } else {
            50 // API 调用失败时的默认值
        }
    };

    let muted = read_mute_from_registry();

    (volume.min(100), muted)
}

/// 从注册表读取静音状态
fn read_mute_from_registry() -> bool {
    unsafe {
        let subkey = windows_sys::core::w!("Software\\Microsoft\\Multimedia\\Audio");
        let mut hkey: HANDLE = std::ptr::null_mut();

        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey, 0, KEY_READ, &mut hkey) != 0 {
            return false;
        }

        let mut muted_val: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let _ = RegQueryValueExW(
            hkey,
            windows_sys::core::w!("UserMute"),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut muted_val as *mut u32 as *mut u8,
            &mut size,
        );

        RegCloseKey(hkey);
        muted_val != 0
    }
}

extern "system" {
    /// winmm.dll — 获取波形音频输出设备的音量
    /// 返回 MMSYSERR_NOERROR (0) 表示成功
    fn waveOutGetVolume(hwo: usize, pdwVolume: *mut u32) -> u32;
}
