//! Sample an existing Scarlet process with the kernel's sync-debug task API.
//! Usage: scarlet_cpu_sample PID [SECONDS]. Resolve PCs with the matching ELF.

#[cfg(target_os = "scarlet")]
fn main() -> Result<(), String> {
    use scarlet_sys::{RawTaskDebugInfoV1, Syscall, TASK_DEBUG_FLAG_PC_VALID};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    let mut arguments = std::env::args().skip(1);
    let pid: usize = arguments
        .next()
        .ok_or("expected PID")?
        .parse()
        .map_err(|_| "invalid PID")?;
    let seconds: u64 = arguments
        .next()
        .unwrap_or_else(|| "20".into())
        .parse()
        .map_err(|_| "invalid seconds")?;
    if pid == 0 || seconds == 0 || seconds > 300 {
        return Err("PID must be positive; duration must be 1..300 seconds".into());
    }
    let mut entries = vec![RawTaskDebugInfoV1::default(); 256];
    let mut previous = HashMap::<u64, u64>::new();
    let mut histogram = HashMap::<(u64, u64, u32, u64), (u64, u64)>::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        // SAFETY: The published v1 ABI takes a live PID selector and a writable
        // array of entries with this exact entry size. It copies snapshots only.
        let count = unsafe {
            scarlet_sys::syscall4(
                Syscall::GetTaskDebugInfo,
                pid,
                entries.as_mut_ptr() as usize,
                entries.len(),
                std::mem::size_of::<RawTaskDebugInfoV1>(),
            )
        };
        if count == usize::MAX || count > entries.len() {
            return Err("task snapshot unavailable (PID or sync-debug kernel)".into());
        }
        if count == entries.len() {
            return Err("task snapshot capacity reached; refusing a truncated sample".into());
        }
        for entry in &entries[..count] {
            if entry.version != scarlet_sys::TASK_DEBUG_INFO_VERSION_V1
                || entry.size as usize != std::mem::size_of::<RawTaskDebugInfoV1>()
            {
                return Err("unexpected task snapshot ABI".into());
            }
            let old = previous.insert(entry.pid, entry.cpu_time_ns);
            if let Some(old) = old {
                let delta = entry.cpu_time_ns.saturating_sub(old);
                if delta > 0 && entry.flags & TASK_DEBUG_FLAG_PC_VALID != 0 {
                    let item = histogram
                        .entry((
                            entry.pid,
                            entry.observed_pc,
                            entry.flags,
                            entry.syscall_number,
                        ))
                        .or_default();
                    item.0 += 1;
                    item.1 += delta;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut rows: Vec<_> = histogram.into_iter().collect();
    rows.sort_unstable_by_key(|(_, (_, cpu))| std::cmp::Reverse(*cpu));
    println!(
        "SCARLET_CPU_SAMPLE pid={pid} seconds={seconds} interval_ms=20 weighted_pc_samples=true"
    );
    for ((tid, pc, flags, syscall), (samples, cpu_ns)) in rows.into_iter().take(60) {
        println!(
            "SCARLET_CPU_SAMPLE tid={tid} pc=0x{pc:x} flags=0x{flags:x} syscall={syscall} samples={samples} cpu_us={}",
            cpu_ns / 1000
        );
    }
    Ok(())
}

#[cfg(not(target_os = "scarlet"))]
fn main() {
    eprintln!("scarlet_cpu_sample requires a Scarlet sync-debug kernel");
    std::process::exit(1);
}
