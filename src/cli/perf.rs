//! `tl perf` — measure the LaunchAgent daemon's CPU/RSS footprint.
//!
//! Shells out to `ps -o %cpu=,rss= -p PID` at `interval_ms` for
//! `duration_secs` and reports min/avg/max. Intended as a quick
//! empirical sanity check: the idle daemon should sit well under
//! 0.1 % CPU after the v0.1.5 backoff work.

use std::io::Write;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::service::{self, ServiceStatus, SystemLaunchctl};

#[derive(Debug, Clone, Copy)]
pub struct PerfOpts {
    pub duration_secs: u64,
    pub interval_ms: u64,
}

impl Default for PerfOpts {
    fn default() -> Self {
        Self { duration_secs: 10, interval_ms: 1000 }
    }
}

pub fn run<W: Write>(cfg: &Config, out: &mut W, opts: PerfOpts) -> Result<()> {
    let pid = resolve_pid()?;
    writeln!(
        out,
        "textlog: sampling daemon pid={pid} for {}s @ {}ms",
        opts.duration_secs, opts.interval_ms,
    )?;

    let deadline = Instant::now() + Duration::from_secs(opts.duration_secs);
    let step = Duration::from_millis(opts.interval_ms);
    let mut cpu_samples: Vec<f64> = Vec::new();
    let mut rss_samples_kb: Vec<u64> = Vec::new();

    while Instant::now() < deadline {
        match sample_ps(pid) {
            Some((cpu, rss_kb)) => {
                cpu_samples.push(cpu);
                rss_samples_kb.push(rss_kb);
            }
            None => {
                writeln!(out, "warn: pid {pid} no longer visible to ps; stopping early")?;
                break;
            }
        }
        thread::sleep(step);
    }

    if cpu_samples.is_empty() {
        return Err(Error::Doctor("no samples collected".into()));
    }

    let cpu = stats_f(&cpu_samples);
    let rss = stats_u(&rss_samples_kb);

    writeln!(out)?;
    writeln!(out, "samples: {}", cpu_samples.len())?;
    writeln!(
        out,
        "cpu%     min {:>6.2}   avg {:>6.2}   max {:>6.2}",
        cpu.0, cpu.1, cpu.2,
    )?;
    writeln!(
        out,
        "rss      min {:>6.1}MB avg {:>6.1}MB max {:>6.1}MB",
        rss.0 as f64 / 1024.0,
        rss.1 / 1024.0,
        rss.2 as f64 / 1024.0,
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "poll interval (config):     {} ms",
        cfg.monitoring.poll_interval_ms,
    )?;
    writeln!(
        out,
        "idle backoff ceiling:       {} ms",
        backoff_ceiling_ms(cfg.monitoring.poll_interval_ms),
    )?;
    writeln!(out)?;

    if cpu.1 < 0.5 {
        writeln!(out, "verdict: idle-grade CPU (avg <0.5%).")?;
    } else if cpu.1 < 2.0 {
        writeln!(
            out,
            "verdict: low CPU (avg <2%); if you were idle during the sample this is already fine.",
        )?;
    } else {
        writeln!(
            out,
            "verdict: higher than expected — is the pipeline actively capturing? Retry with no clipboard activity.",
        )?;
    }

    Ok(())
}

fn backoff_ceiling_ms(poll_interval_ms: u64) -> u64 {
    // Mirror `monitor_loop`: ceiling = 4× base, capped at 2 s.
    poll_interval_ms.saturating_mul(4).min(2000)
}

fn resolve_pid() -> Result<u32> {
    match service::status(&SystemLaunchctl)? {
        ServiceStatus::NotInstalled => Err(Error::Launchctl(
            "daemon not installed — run `tl install` first".into(),
        )),
        ServiceStatus::Installed { loaded: false, .. } => Err(Error::Launchctl(
            "daemon installed but not loaded — run `tl start`".into(),
        )),
        ServiceStatus::Installed { pid: None, .. } => Err(Error::Launchctl(
            "daemon loaded but has no pid — likely mid-respawn; try again in a moment".into(),
        )),
        ServiceStatus::Installed { pid: Some(pid), .. } => Ok(pid),
    }
}

fn sample_ps(pid: u32) -> Option<(f64, u64)> {
    let out = Command::new("ps")
        .args(["-o", "%cpu=", "-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.split_whitespace();
    let cpu: f64 = parts.next()?.parse().ok()?;
    let rss: u64 = parts.next()?.parse().ok()?;
    Some((cpu, rss))
}

fn stats_f(v: &[f64]) -> (f64, f64, f64) {
    let min = v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let avg = v.iter().sum::<f64>() / v.len() as f64;
    (min, avg, max)
}

fn stats_u(v: &[u64]) -> (u64, f64, u64) {
    let min = *v.iter().min().expect("non-empty checked by caller");
    let max = *v.iter().max().expect("non-empty checked by caller");
    let avg = v.iter().sum::<u64>() as f64 / v.len() as f64;
    (min, avg, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_f_single_sample_is_the_sample() {
        let (mn, av, mx) = stats_f(&[0.3]);
        assert_eq!(mn, 0.3);
        assert!((av - 0.3).abs() < f64::EPSILON);
        assert_eq!(mx, 0.3);
    }

    #[test]
    fn stats_u_computes_min_avg_max() {
        let (mn, av, mx) = stats_u(&[100, 200, 300]);
        assert_eq!(mn, 100);
        assert!((av - 200.0).abs() < f64::EPSILON);
        assert_eq!(mx, 300);
    }

    #[test]
    fn backoff_ceiling_is_four_x_up_to_two_seconds() {
        assert_eq!(backoff_ceiling_ms(250), 1000);
        assert_eq!(backoff_ceiling_ms(500), 2000);
        assert_eq!(backoff_ceiling_ms(1000), 2000);
        assert_eq!(backoff_ceiling_ms(5000), 2000);
    }
}
