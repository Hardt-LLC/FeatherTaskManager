#![windows_subsystem = "windows"]
#![cfg_attr(not(target_os = "windows"), allow(unused))]

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
compile_error!("Feather Task Manager currently targets 64-bit Windows (x86_64-pc-windows-msvc).");

mod actions;
mod performance;
mod sampler;
mod services;
mod startup;
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--test-child") {
        std::thread::sleep(std::time::Duration::from_secs(30));
        return;
    }
    if args.get(1).map(String::as_str) == Some("--render-previews") {
        let directory = std::path::Path::new(args.get(2).map(String::as_str).unwrap_or("previews"));
        if let Err(error) = ui::render_previews(directory) {
            let _ = std::fs::create_dir_all(directory);
            let _ = std::fs::write(directory.join("error.txt"), error);
            std::process::exit(1);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--self-test") {
        let result = self_test();
        let output = args.get(2).map(String::as_str).unwrap_or("self-test.txt");
        let passed = result.is_ok();
        let text = match result {
            Ok(s) => s,
            Err(e) => format!("FAIL: {e}\n"),
        };
        if std::fs::write(output, text).is_err() || !passed {
            std::process::exit(1);
        }
        return;
    }
    ui::run();
}

fn self_test() -> Result<String, String> {
    use std::time::{Duration, Instant};
    let mut sampler = sampler::Sampler::new()?;
    let t = Instant::now();
    let first = sampler.sample()?;
    let first_ms = t.elapsed().as_secs_f64() * 1000.0;
    let own_pid = std::process::id();
    let own = first
        .processes
        .iter()
        .find(|p| p.pid == own_pid)
        .ok_or("own process missing")?;
    if own.working_set == 0 || own.created == 0 {
        return Err("invalid own process counters".into());
    }
    let path = actions::executable_path(own.pid, own.created)?;
    if actions::terminate(own.pid, own.created).is_ok() {
        return Err("self termination was allowed".into());
    }
    if actions::executable_path(own.pid, own.created.wrapping_add(1)).is_ok() {
        return Err("PID reuse guard failed".into());
    }
    if actions::terminate(4, 1).is_ok() {
        return Err("System termination was allowed".into());
    }
    let mut child = std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .arg("--test-child")
        .spawn()
        .map_err(|e| e.to_string())?;
    let child_result = (|| {
        let snap = sampler.sample()?;
        let proc = snap
            .processes
            .iter()
            .find(|p| p.pid == child.id())
            .ok_or("test child missing")?;
        actions::terminate(proc.pid, proc.created.wrapping_add(1))
            .err()
            .ok_or("stale identity accepted")?;
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            return Err("stale identity killed child".into());
        }
        actions::terminate(proc.pid, proc.created)?;
        child.wait().map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    })();
    if child_result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    child_result?;
    let mut costs = Vec::new();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(50));
        let snap = sampler.sample()?;
        if !(0.0..=100.0).contains(&snap.cpu_percent)
            || snap.memory_total == 0
            || snap.memory_used > snap.memory_total
        {
            return Err("invalid system counters".into());
        }
        if snap.processes.iter().any(|p| {
            !p.cpu_percent.is_finite()
                || !(0.0..=100.0).contains(&p.cpu_percent)
                || !p.io_bytes_per_sec.is_finite()
        }) {
            return Err("invalid process counters".into());
        }
        costs.push(snap.sample_ms);
    }
    costs.sort_by(f64::total_cmp);
    let service_count = services::list()?.len();
    let startup_count = startup::list()?.len();
    let mut perf = performance::PerfSampler::new()?;
    let _ = perf.sample()?;
    std::thread::sleep(Duration::from_millis(1100));
    let perf = perf.sample()?;
    if perf.logical_cpus == 0 || perf.cpu_name.is_empty() {
        return Err("invalid performance metadata".into());
    }
    Ok(format!("PASS\nprocesses={}\nservices={}\nstartup_entries={}\nfirst_snapshot_ms={first_ms:.3}\nsample_median_ms={:.3}\nsample_max_ms={:.3}\nexecutable={}\nperformance_cpu={}\nperformance_disk_ready={}\nperformance_network_ready={}\nperformance_gpu={:?}\nperformance_warnings={:?}\nChecks: live snapshots, own identity, memory bounds, finite CPU/I/O, stale PID rejection, System/self protection, disposable child termination, read-only service/startup enumeration, native performance providers.\n", first.processes.len(), service_count, startup_count, costs[costs.len()/2], costs[costs.len()-1], path, perf.cpu_name, perf.disk_rates_ready, perf.network_rates_ready, perf.gpu_percent, perf.warnings))
}
