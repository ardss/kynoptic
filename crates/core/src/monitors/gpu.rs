//! GPU 状态快照
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 数据来自两个源，按 GPU name 合并：
//!   - Win32_VideoController：静态信息（型号 / 驱动 / 分辨率 / 标称显存）
//!   - nvidia-smi：动态指标（利用率 / 温度 / 已用显存 / 总显存），仅 NVIDIA 卡可用
//!
//! 每次都发送（不再按静态 key 去重）—— 利用率/温度是实时值，去重会让前端永远看不到变化。
//! 只有当枚举出的 GPU 集合完全相同（含型号/驱动/分辨率）时才跳过，避免无意义写入。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct GpuMonitor {
    /// 上一次枚举到的静态 GPU 指纹，用于「GPU 列表本身没变则不重复发静态部分」的去重。
    prev_static_key: Cell<Option<String>>,
}

impl Default for GpuMonitor {
    fn default() -> Self {
        Self {
            prev_static_key: Cell::new(None),
        }
    }
}

impl Monitor for GpuMonitor {
    fn name(&self) -> &str {
        "gpu"
    }
    fn interval(&self) -> Duration {
        // 缩短到 10s：利用率/温度是热路径指标，前端 3s 轮询，120s 间隔会让 GPU 卡长时间无更新。
        Duration::from_secs(10)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        // 1) 静态信息：型号 / AdapterRAM / 刷新率 / 驱动 / 分辨率
        let script = r#"
Get-CimInstance Win32_VideoController -Namespace root/CIMV2 -ErrorAction SilentlyContinue |
  ForEach-Object {
    "$($_.Name)|$($_.AdapterRAM)|$($_.CurrentRefreshRate)|$($_.DriverVersion)|$($_.VideoModeDescription)"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let mut gpus: Vec<GpuInfo> = Vec::new();
        let mut static_key = String::new();

        for line in output.trim().lines() {
            let parts: Vec<&str> = line.splitn(5, '|').collect();
            if parts.len() >= 5 {
                let name = parts[0].trim();
                let vram = parts[1].trim().parse::<u64>().unwrap_or(0);
                let refresh = parts[2].trim().parse::<u32>().unwrap_or(0);
                let driver = parts[3].trim();
                let mode = parts[4].trim();
                static_key.push_str(&format!("{}|{}|{}|{}|", name, vram, driver, mode));

                gpus.push(GpuInfo {
                    name: name.to_string(),
                    vram_mb: vram / (1024 * 1024),
                    refresh_rate: refresh,
                    driver_version: driver.to_string(),
                    video_mode: mode.to_string(),
                    utilization: None,
                    temperature: None,
                    memory_used_mb: None,
                    memory_total_mb: None,
                });
            }
        }

        if gpus.is_empty() {
            return;
        }

        // 2) 动态指标：NVIDIA 卡用 nvidia-smi 补 utilization/temperature/memory_*_mb。
        //    非 NVIDIA 或无 nvidia-smi 时静默跳过，对应字段保持 None（序列化省略，前端显示「—」）。
        let nvidia_stats = query_nvidia_smi();
        for gpu in gpus.iter_mut() {
            if gpu.name.to_lowercase().contains("nvidia") {
                if let Some(stats) = nvidia_stats
                    .iter()
                    .find(|s| gpu.name.to_lowercase().contains(&s.name.to_lowercase()))
                {
                    gpu.utilization = stats.utilization;
                    gpu.temperature = stats.temperature;
                    gpu.memory_used_mb = stats.memory_used_mb;
                    gpu.memory_total_mb = stats.memory_total_mb;
                }
            }
        }

        // 3) 去重：仅当静态指纹不变时跳过。动态字段不参与指纹，保证利用率变化能正常上报。
        //    为避免「指纹稳定后 GPU 利用率永远停留在最后一次写入」的死锁，改为：
        //    只要本轮拿到了 nvidia-smi 动态值，就总是发送。
        let has_dynamic = nvidia_stats.iter().any(|s| {
            gpus.iter()
                .any(|g| g.name.to_lowercase().contains(&s.name.to_lowercase()))
        });

        if !has_dynamic {
            let prev = self.prev_static_key.take();
            if prev.as_ref() == Some(&static_key) {
                self.prev_static_key.set(prev);
                return;
            }
            self.prev_static_key.set(Some(static_key));
        }

        // 直接序列化 GpuSnapshot struct —— 字段名单一来源。
        let snap = GpuSnapshot { gpus };
        let data = serde_json::to_value(&snap).unwrap_or_else(|_| json!({}));
        let event = Event::new(EventAction::GpuSnapshot, EventType::System).data(data);
        let _ = tx.try_send(event);
    }
}

/// 单张 GPU 信息（gpu_snapshot.gpus 元素）。
///
/// 字段名即契约：前端 HardwareView 读 name/utilization/temperature/memory_used_mb/
/// memory_total_mb。静态字段总有值；动态字段（utilization/temperature/memory_*）
/// 仅 NVIDIA 卡经 nvidia-smi 采到时为 Some，否则 None → 序列化省略，前端显示「—」。
#[derive(serde::Serialize)]
struct GpuInfo {
    name: String,
    vram_mb: u64,
    refresh_rate: u32,
    driver_version: String,
    video_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    utilization: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_used_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_total_mb: Option<u64>,
}

/// gpu_snapshot 顶层结构。
#[derive(serde::Serialize)]
struct GpuSnapshot {
    gpus: Vec<GpuInfo>,
}

/// nvidia-smi 单卡动态指标。
struct NvidiaStat {
    name: String,
    utilization: Option<u32>,
    temperature: Option<i32>,
    memory_used_mb: Option<u64>,
    memory_total_mb: Option<u64>,
}

/// 调用 `nvidia-smi --query-gpu=... --format=csv,noheader,nounits` 拉取每张 NVIDIA 卡的
/// 利用率 / 温度 / 显存。失败或无 NVIDIA 驱动时返回空 Vec（调用方静默降级）。
fn query_nvidia_smi() -> Vec<NvidiaStat> {
    let output = match std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,temperature.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Vec::new(),
    };
    parse_nvidia_smi(&output)
}

/// 纯函数：解析 nvidia-smi CSV 输出（`--format=csv,noheader,nounits`）。
///
/// 每行格式：`name,utilization.gpu,temperature.gpu,memory.used,memory.total`
/// 非数字字段（如 `[N/A]`）解析为 None。便于单元测试多卡/异常值场景。
fn parse_nvidia_smi(output: &str) -> Vec<NvidiaStat> {
    let parse_num = |s: &str| -> Option<u64> {
        let s = s.trim();
        if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        s.parse::<u64>().ok()
    };

    output
        .trim()
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if parts.len() < 5 {
                return None;
            }
            Some(NvidiaStat {
                name: parts[0].to_string(),
                utilization: parse_num(parts[1]).map(|v| v as u32),
                temperature: parse_num(parts[2]).map(|v| v as i32),
                memory_used_mb: parse_num(parts[3]),
                memory_total_mb: parse_num(parts[4]),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_nvidia_smi_single_card() {
        let out = "NVIDIA GeForce RTX 5060, 29, 57, 2718, 8151\n";
        let stats = parse_nvidia_smi(out);
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.name, "NVIDIA GeForce RTX 5060");
        assert_eq!(s.utilization, Some(29));
        assert_eq!(s.temperature, Some(57));
        assert_eq!(s.memory_used_mb, Some(2718));
        assert_eq!(s.memory_total_mb, Some(8151));
    }

    #[test]
    fn parse_nvidia_smi_na_values() {
        // 某些指标可能为 [N/A]（如休眠中的卡），应解析为 None
        let out = "NVIDIA GeForce RTX 5060, [N/A], [N/A], 100, 8151\n";
        let stats = parse_nvidia_smi(out);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].utilization, None);
        assert_eq!(stats[0].temperature, None);
        assert_eq!(stats[0].memory_used_mb, Some(100));
    }

    #[test]
    fn parse_nvidia_smi_empty_and_malformed() {
        assert!(parse_nvidia_smi("").is_empty());
        // 不足 5 列的行应被跳过
        let out = "bad,line\nNVIDIA GeForce RTX 5060, 29, 57, 2718, 8151\n";
        let stats = parse_nvidia_smi(out);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].utilization, Some(29));
    }

    /// 契约测试：GpuInfo 序列化键名必须与前端 HardwareView GPU 卡一致。
    #[test]
    fn gpu_info_contract_keys_with_dynamic() {
        let gpu = GpuInfo {
            name: "NVIDIA GeForce RTX 5060".into(),
            vram_mb: 8,
            refresh_rate: 60,
            driver_version: "32.0.15.9186".into(),
            video_mode: "2560x1440".into(),
            utilization: Some(29),
            temperature: Some(57),
            memory_used_mb: Some(2718),
            memory_total_mb: Some(8151),
        };
        let v = serde_json::to_value(&gpu).unwrap();
        let obj = v.as_object().unwrap();
        // 前端读的字段
        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("utilization"));
        assert!(obj.contains_key("temperature"));
        assert!(obj.contains_key("memory_used_mb"));
        assert!(obj.contains_key("memory_total_mb"));
    }

    #[test]
    fn gpu_info_dynamic_fields_omitted_when_none() {
        // 虚拟显示器（非 NVIDIA）动态字段全 None → 序列化省略
        let gpu = GpuInfo {
            name: "OrayIddDriver Device".into(),
            vram_mb: 0,
            refresh_rate: 0,
            driver_version: "17.50.19.949".into(),
            video_mode: String::new(),
            utilization: None,
            temperature: None,
            memory_used_mb: None,
            memory_total_mb: None,
        };
        let v = serde_json::to_value(&gpu).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("utilization"), "None 字段应省略");
        assert!(!obj.contains_key("temperature"));
        assert!(obj.contains_key("name"), "静态字段应保留");
    }
}
