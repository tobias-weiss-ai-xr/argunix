//! Cheap `/proc` sampler for the heartbeat payload.
//!
//! Linux-only. Returns `None` on read/parse failure so the heartbeat
//! itself remains a pure liveness signal — sampling problems must
//! never knock the agent off the control channel.

use argunix_builders::BuilderStats;

/// Stateful sampler. CPU% requires a delta against the prior sample,
/// so we keep the last raw `/proc/stat` totals; the first sample
/// after agent start reports cpu=0.
#[derive(Default)]
pub struct StatsSampler {
    prev_cpu: Option<CpuTotals>,
}

#[derive(Debug, Clone, Copy)]
struct CpuTotals {
    idle: u64,
    total: u64,
}

impl StatsSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample loadavg, meminfo, and (relative to the prior sample) cpu%.
    /// Returns `None` if any of the `/proc` reads fail or parse;
    /// callers should treat that as "no stats this tick" without
    /// retrying within the tick.
    pub fn sample(&mut self) -> Option<BuilderStats> {
        let load1 = read_loadavg()?;
        let (mem_used_bytes, mem_total_bytes) = read_meminfo()?;
        let now = read_cpu_totals()?;
        let cpu_percent = match self.prev_cpu {
            None => 0.0,
            Some(prev) => cpu_busy_percent(prev, now),
        };
        self.prev_cpu = Some(now);
        Some(BuilderStats {
            load1,
            cpu_percent,
            mem_used_bytes,
            mem_total_bytes,
        })
    }
}

fn read_loadavg() -> Option<f32> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    s.split_whitespace().next()?.parse::<f32>().ok()
}

fn read_meminfo() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kb = None;
    let mut avail_kb = None;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = parse_kb(rest);
        }
        if total_kb.is_some() && avail_kb.is_some() {
            break;
        }
    }
    let total_kb = total_kb?;
    let avail_kb = avail_kb?;
    let used_kb = total_kb.saturating_sub(avail_kb);
    Some((used_kb * 1024, total_kb * 1024))
}

fn parse_kb(rest: &str) -> Option<u64> {
    rest.trim()
        .strip_suffix("kB")
        .or_else(|| rest.trim().strip_suffix("KB"))
        .map(|n| n.trim())
        .and_then(|n| n.parse::<u64>().ok())
}

fn read_cpu_totals() -> Option<CpuTotals> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let line = s.lines().next()?;
    // `cpu  user nice system idle iowait irq softirq steal guest guest_nice`
    // Treat idle+iowait as not-busy (matches what `top` does).
    let mut parts = line.split_whitespace();
    if parts.next()? != "cpu" {
        return None;
    }
    let nums: Vec<u64> = parts.filter_map(|p| p.parse::<u64>().ok()).collect();
    if nums.len() < 4 {
        return None;
    }
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let total: u64 = nums.iter().sum();
    Some(CpuTotals { idle, total })
}

fn cpu_busy_percent(prev: CpuTotals, now: CpuTotals) -> f32 {
    let total_delta = now.total.saturating_sub(prev.total);
    if total_delta == 0 {
        return 0.0;
    }
    let idle_delta = now.idle.saturating_sub(prev.idle);
    let busy = total_delta.saturating_sub(idle_delta);
    (busy as f32 / total_delta as f32) * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kb_handles_leading_whitespace_and_units() {
        assert_eq!(parse_kb("       16384 kB"), Some(16384));
        assert_eq!(parse_kb(" 16384 KB"), Some(16384));
        assert_eq!(parse_kb("16384"), None);
    }

    #[test]
    fn cpu_busy_percent_zero_total_yields_zero() {
        let same = CpuTotals {
            idle: 5,
            total: 100,
        };
        assert_eq!(cpu_busy_percent(same, same), 0.0);
    }

    #[test]
    fn cpu_busy_percent_all_idle_is_zero() {
        let prev = CpuTotals {
            idle: 50,
            total: 100,
        };
        let now = CpuTotals {
            idle: 60,
            total: 110,
        };
        assert_eq!(cpu_busy_percent(prev, now), 0.0);
    }

    #[test]
    fn cpu_busy_percent_half_busy_is_fifty() {
        let prev = CpuTotals {
            idle: 50,
            total: 100,
        };
        let now = CpuTotals {
            idle: 55,
            total: 110,
        };
        assert!((cpu_busy_percent(prev, now) - 50.0).abs() < 0.01);
    }

    #[test]
    fn first_sample_reports_zero_cpu() {
        // Can't unit-test live /proc parsing portably, but we can pin
        // the stateful contract: a brand-new sampler reports cpu=0
        // even when /proc reads succeed (because no prior to diff
        // against). This relies on /proc being available at test
        // runtime; gate it so the test is meaningful only on Linux.
        if !std::path::Path::new("/proc/stat").exists() {
            return;
        }
        let mut s = StatsSampler::new();
        let s1 = s.sample().expect("sampling /proc must work on Linux CI");
        assert_eq!(s1.cpu_percent, 0.0, "first sample reports 0% cpu");
    }
}
