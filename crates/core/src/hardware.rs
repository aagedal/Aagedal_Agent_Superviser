//! How much memory a model may use on this machine.

use serde::Serialize;

const GIB: u64 = 1 << 30;

/// Memory left for the OS, the harness and a browser when nothing else limits the model.
const RESERVED: u64 = 4 * GIB;

#[derive(Debug, Clone, Serialize)]
pub struct Hardware {
    pub total_memory: u64,
    /// Bytes a loaded model (weights, KV cache and buffers) may take.
    pub model_budget: u64,
    /// True on Apple Silicon, where the GPU shares system memory.
    pub unified_memory: bool,
    pub os: &'static str,
    pub arch: &'static str,
}

impl Hardware {
    pub fn detect() -> Self {
        let total = sysinfo::System::new()
            .map(|mut system| {
                system.refresh_memory();
                system.total_memory()
            })
            .unwrap_or(0);
        let unified = cfg!(all(target_os = "macos", target_arch = "aarch64"));
        let wired_limit = if unified { macos_wired_limit() } else { None };
        Self {
            total_memory: total,
            model_budget: model_budget(total, unified, wired_limit),
            unified_memory: unified,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }
}

/// On Apple Silicon, Metal may wire about two thirds of memory up to 36 GB and three quarters
/// above, unless `iogpu.wired_limit_mb` raises it. Elsewhere the model runs from system RAM
/// (or spills to it), so we keep a fixed reserve. Discrete-GPU VRAM is not considered yet.
pub fn model_budget(total: u64, unified: bool, wired_limit: Option<u64>) -> u64 {
    let after_reserve = total.saturating_sub(RESERVED);
    if !unified {
        return after_reserve;
    }
    let gpu_limit = wired_limit.unwrap_or(if total <= 36 * GIB { total / 3 * 2 } else { total / 4 * 3 });
    gpu_limit.min(after_reserve.max(total / 2))
}

fn macos_wired_limit() -> Option<u64> {
    let out = std::process::Command::new("/usr/sbin/sysctl").args(["-n", "iogpu.wired_limit_mb"]).output().ok()?;
    let mb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (mb > 0).then_some(mb * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_silicon_budgets() {
        // 8 GB: the 4 GB reserve is tighter than the GPU limit.
        assert_eq!(model_budget(8 * GIB, true, None), 4 * GIB);
        assert_eq!(model_budget(16 * GIB, true, None), 16 * GIB / 3 * 2);
        assert_eq!(model_budget(64 * GIB, true, None), 48 * GIB);
        assert_eq!(model_budget(64 * GIB, true, Some(56 * GIB)), 56 * GIB);
        // A raised wired limit still leaves the reserve.
        assert_eq!(model_budget(64 * GIB, true, Some(64 * GIB)), 60 * GIB);
    }

    #[test]
    fn other_platforms_keep_a_reserve() {
        assert_eq!(model_budget(32 * GIB, false, None), 28 * GIB);
        assert_eq!(model_budget(2 * GIB, false, None), 0);
    }
}
