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
    /// Bytes llama-server may take in all: the loaded model and what it keeps in plain RAM,
    /// which the GPU limit behind `model_budget` doesn't apply to.
    pub ram_budget: u64,
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
            ram_budget: ram_budget(total, unified),
            unified_memory: unified,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }

    /// The budgets left once `bytes` go to something else running beside the model.
    pub fn less(&self, bytes: u64) -> Self {
        Self { model_budget: self.model_budget.saturating_sub(bytes), ram_budget: self.ram_budget.saturating_sub(bytes), ..self.clone() }
    }
}

/// On Apple Silicon, Metal may wire about two thirds of memory up to 36 GB and three quarters
/// above, unless `iogpu.wired_limit_mb` raises it. Elsewhere the model runs from system RAM
/// (or spills to it), so we keep a fixed reserve. Discrete-GPU VRAM is not considered yet.
pub fn model_budget(total: u64, unified: bool, wired_limit: Option<u64>) -> u64 {
    let ram = ram_budget(total, unified);
    if !unified {
        return ram;
    }
    let gpu_limit = wired_limit.unwrap_or(if total <= 36 * GIB { total / 3 * 2 } else { total / 4 * 3 });
    gpu_limit.min(ram)
}

/// Memory left after the reserve; on small Apple Silicon machines, at least half of it.
pub fn ram_budget(total: u64, unified: bool) -> u64 {
    let after_reserve = total.saturating_sub(RESERVED);
    if unified { after_reserve.max(total / 2) } else { after_reserve }
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
    fn ram_budget_is_beyond_the_gpu_limit() {
        assert_eq!(ram_budget(64 * GIB, true), 60 * GIB);
        assert_eq!(ram_budget(16 * GIB, true), 12 * GIB);
        assert_eq!(ram_budget(8 * GIB, true), 4 * GIB);
        assert_eq!(ram_budget(32 * GIB, false), 28 * GIB);
    }

    #[test]
    fn less_takes_from_both_budgets() {
        let hardware = Hardware { total_memory: 16 * GIB, model_budget: 10 * GIB, ram_budget: 12 * GIB, unified_memory: true, os: "macos", arch: "aarch64" };
        let left = hardware.less(5 * GIB);
        assert_eq!((left.model_budget, left.ram_budget, left.total_memory), (5 * GIB, 7 * GIB, 16 * GIB));
        assert_eq!(hardware.less(20 * GIB).model_budget, 0);
    }

    #[test]
    fn other_platforms_keep_a_reserve() {
        assert_eq!(model_budget(32 * GIB, false, None), 28 * GIB);
        assert_eq!(model_budget(2 * GIB, false, None), 0);
    }
}
