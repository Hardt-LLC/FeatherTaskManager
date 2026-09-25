//! A bulk process snapshot: no per-process handles, WMI, PDH, or background work.
//!
//! SYSTEM_PROCESS_INFORMATION is an NT ABI, so resolve its entry point at runtime
//! and validate every record and string before reading. The x64 layout is shared
//! by supported Windows 10/11 releases. See Microsoft's NtQuerySystemInformation
//! documentation and the System Informer phnt ntexapi.h field definitions.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::time::Instant;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{GetActiveProcessorCount, GetSystemTimes};

#[cfg(not(target_pointer_width = "64"))]
compile_error!("The native process sampler requires a 64-bit Windows build.");

type NtQuerySystemInformation = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;

const SYSTEM_PROCESS_INFORMATION: u32 = 5;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xc0000004_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xc0000023_u32 as i32;
const HEADER_BYTES: usize = 256;
const INITIAL_BUFFER_BYTES: usize = 256 * 1024;
const MAX_BUFFER_BYTES: usize = 64 * 1024 * 1024;
const TICKS_PER_SECOND: f64 = 10_000_000.0;

#[derive(Clone, Debug)]
pub struct Process {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
    /// Process creation timestamp in Windows 100 ns units; identifies PID reuse.
    pub created: u64,
    /// Percent of the entire machine, not percent of a single logical CPU.
    pub cpu_percent: f64,
    pub working_set: u64,
    /// Private committed bytes; this is not the private resident working set.
    pub private_bytes: u64,
    /// All read, write and other I/O transfer bytes per second (not disk only).
    pub io_bytes_per_sec: f64,
    pub threads: u32,
    pub handles: u32,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub processes: Vec<Process>,
    pub cpu_percent: f64,
    pub memory_used: u64,
    pub memory_total: u64,
    /// Time spent collecting and decoding this snapshot, in milliseconds.
    pub sample_ms: f64,
}

#[derive(Clone, Copy, Default)]
struct Counters {
    created: u64,
    cpu: u64,
    io: u64,
    seen: u64,
}

struct ProcessSample {
    process: Process,
    counters: Counters,
}

#[derive(Clone, Copy)]
struct SystemTimes {
    idle: u64,
    total: u64,
}

pub struct Sampler {
    query: NtQuerySystemInformation,
    // u128 guarantees the alignment required by the native API; the allocation
    // grows only if the machine's process/thread list outgrows its capacity.
    buffer: Vec<u128>,
    previous: HashMap<u32, Counters>,
    previous_at: Option<Instant>,
    previous_system: Option<SystemTimes>,
    generation: u64,
    processors: u32,
}

impl Sampler {
    pub fn new() -> Result<Self, String> {
        let module_name: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
        // ntdll is already loaded for every Win32 process; no DLL is loaded from
        // the current directory and no module ownership needs to be released.
        let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
        if module.is_null() {
            return Err(format!(
                "Cannot locate ntdll: {}",
                std::io::Error::last_os_error()
            ));
        }
        let address =
            unsafe { GetProcAddress(module, c"NtQuerySystemInformation".as_ptr().cast()) }
                .ok_or_else(|| {
                    "NtQuerySystemInformation is unavailable on this Windows version".to_owned()
                })?;
        // The symbol's documented NTAPI signature is exactly the alias above.
        let query = unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, NtQuerySystemInformation>(
                address,
            )
        };
        let processors = unsafe { GetActiveProcessorCount(u16::MAX) };
        if processors == 0 {
            return Err(format!(
                "Cannot read CPU count: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut sampler = Self {
            query,
            buffer: Vec::new(),
            previous: HashMap::with_capacity(256),
            previous_at: None,
            previous_system: None,
            generation: 0,
            processors,
        };
        sampler.resize_buffer(INITIAL_BUFFER_BYTES)?;
        Ok(sampler)
    }

    pub fn sample(&mut self) -> Result<Snapshot, String> {
        let started = Instant::now();
        let valid_bytes = self.query_processes()?;
        let sampled_at = Instant::now();
        // GetSystemTimes reports only the calling thread's processor group on
        // machines above 64 CPUs; use the system-wide idle process there instead.
        let system = if self.processors <= 64 {
            Some(read_system_times()?)
        } else {
            None
        };
        let mut memory: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        memory.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut memory) } == 0 {
            return Err(format!(
                "Cannot read memory status: {}",
                std::io::Error::last_os_error()
            ));
        }
        // All elements are initialized, the byte slice never exceeds the Vec's
        // length, and no pointer into it escapes parse_processes.
        let bytes =
            unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), valid_bytes) };
        let samples = parse_processes(bytes, self.previous.len())?;
        let seconds = self
            .previous_at
            .map_or(0.0, |then| sampled_at.duration_since(then).as_secs_f64());
        let cpu_budget = seconds * TICKS_PER_SECOND * self.processors as f64;
        let mut cpu_percent = match (self.previous_system, system) {
            (Some(before), Some(now)) => system_cpu_percent(before, now),
            _ => 0.0,
        };
        self.generation = self.generation.wrapping_add(1);
        let mut processes = Vec::with_capacity(samples.len());
        for mut sample in samples {
            let pid = sample.process.pid;
            let before = self.previous.get(&pid).copied();
            let (cpu, io) = rates(before, sample.counters, seconds, cpu_budget);
            sample.counters.seen = self.generation;
            self.previous.insert(pid, sample.counters);
            if pid == 0 {
                if self.processors > 64 && before.is_some() && seconds > 0.0 {
                    cpu_percent = (100.0 - cpu).clamp(0.0, 100.0);
                }
                continue;
            }
            sample.process.cpu_percent = cpu;
            sample.process.io_bytes_per_sec = io;
            processes.push(sample.process);
        }
        self.previous
            .retain(|_, counters| counters.seen == self.generation);
        self.previous_at = Some(sampled_at);
        self.previous_system = system;
        Ok(Snapshot {
            processes,
            cpu_percent,
            memory_used: memory.ullTotalPhys.saturating_sub(memory.ullAvailPhys),
            memory_total: memory.ullTotalPhys,
            sample_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    }

    fn resize_buffer(&mut self, bytes: usize) -> Result<(), String> {
        if bytes > MAX_BUFFER_BYTES {
            return Err("Process snapshot exceeds the 64 MiB safety limit".to_owned());
        }
        let words = bytes.div_ceil(size_of::<u128>());
        self.buffer
            .try_reserve_exact(words.saturating_sub(self.buffer.len()))
            .map_err(|_| "Not enough memory for the process snapshot".to_owned())?;
        self.buffer.resize(words, 0);
        Ok(())
    }

    fn query_processes(&mut self) -> Result<usize, String> {
        // Bounded retries protect the UI from unbounded process-creation races.
        for _ in 0..8 {
            let bytes = self.buffer.len() * size_of::<u128>();
            let mut required = 0u32;
            let status = unsafe {
                (self.query)(
                    SYSTEM_PROCESS_INFORMATION,
                    self.buffer.as_mut_ptr().cast(),
                    bytes as u32,
                    &mut required,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH || status == STATUS_BUFFER_TOO_SMALL {
                let next = (required as usize)
                    .saturating_add(64 * 1024)
                    .max(bytes.saturating_mul(3) / 2);
                self.resize_buffer(next)?;
                continue;
            }
            if status < 0 {
                return Err(format!(
                    "Process query failed (NTSTATUS 0x{:08X})",
                    status as u32
                ));
            }
            if required as usize > bytes || (required as usize) < HEADER_BYTES {
                return Err("Windows returned an invalid process snapshot size".to_owned());
            }
            return Ok(required as usize);
        }
        Err("Process list changed too quickly; retry on the next refresh".to_owned())
    }
}

fn read_system_times() -> Result<SystemTimes, String> {
    let mut idle: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return Err(format!(
            "Cannot read CPU times: {}",
            std::io::Error::last_os_error()
        ));
    }
    let ticks =
        |value: FILETIME| (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
    Ok(SystemTimes {
        idle: ticks(idle),
        total: ticks(kernel).saturating_add(ticks(user)),
    })
}

fn system_cpu_percent(before: SystemTimes, now: SystemTimes) -> f64 {
    let Some(total) = now.total.checked_sub(before.total).filter(|&n| n > 0) else {
        return 0.0;
    };
    let Some(idle) = now.idle.checked_sub(before.idle) else {
        return 0.0;
    };
    100.0 * total.saturating_sub(idle) as f64 / total as f64
}

fn rates(before: Option<Counters>, now: Counters, seconds: f64, cpu_budget: f64) -> (f64, f64) {
    let Some(before) = before.filter(|old| old.created == now.created) else {
        return (0.0, 0.0);
    };
    if seconds <= 0.0 || cpu_budget <= 0.0 {
        return (0.0, 0.0);
    }
    let cpu = now.cpu.saturating_sub(before.cpu) as f64 / cpu_budget * 100.0;
    let io = now.io.saturating_sub(before.io) as f64 / seconds;
    (cpu.clamp(0.0, 100.0), io)
}

fn parse_processes(bytes: &[u8], previous_count: usize) -> Result<Vec<ProcessSample>, String> {
    let invalid = || "Windows returned a malformed native process record".to_owned();
    if bytes.len() < HEADER_BYTES {
        return Err(invalid());
    }
    let mut samples = Vec::with_capacity(previous_count.saturating_add(16));
    let mut offset = 0usize;
    loop {
        let remaining = bytes.get(offset..).ok_or_else(invalid)?;
        let header = remaining.get(..HEADER_BYTES).ok_or_else(invalid)?;
        // The full header is checked once. All offsets below are fixed x64 ABI
        // fields and are decoded as bytes, avoiding alignment/padding UB.
        let u16_at = |n| u16::from_le_bytes(header[n..n + 2].try_into().unwrap());
        let u32_at = |n| u32::from_le_bytes(header[n..n + 4].try_into().unwrap());
        let u64_at = |n| u64::from_le_bytes(header[n..n + 8].try_into().unwrap());
        let next = u32_at(0) as usize;
        let entry_size = if next == 0 { remaining.len() } else { next };
        if entry_size < HEADER_BYTES
            || entry_size > remaining.len()
            || (next != 0 && !next.is_multiple_of(8))
        {
            return Err(invalid());
        }
        let pid = u32::try_from(u64_at(80)).map_err(|_| invalid())?;
        let parent_pid = u32::try_from(u64_at(88)).map_err(|_| invalid())?;
        let name_length = u16_at(56) as usize;
        let max_name_length = u16_at(58) as usize;
        let name = if name_length == 0 {
            match pid {
                0 => "System Idle Process".to_owned(),
                4 => "System".to_owned(),
                _ => format!("PID {pid}"),
            }
        } else {
            if !name_length.is_multiple_of(2) || max_name_length < name_length {
                return Err(invalid());
            }
            let name_address = usize::try_from(u64_at(64)).map_err(|_| invalid())?;
            let name_offset = name_address
                .checked_sub(bytes.as_ptr() as usize)
                .ok_or_else(invalid)?;
            let name_end = name_offset.checked_add(name_length).ok_or_else(invalid)?;
            // A name must live in its own record, never point outside the query
            // buffer, into another record, or into a separately mapped address.
            if name_offset < offset + HEADER_BYTES || name_end > offset + entry_size {
                return Err(invalid());
            }
            let encoded = bytes.get(name_offset..name_end).ok_or_else(invalid)?;
            char::decode_utf16(
                encoded
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]])),
            )
            .map(|character| character.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
        };
        let created = u64_at(32);
        samples.push(ProcessSample {
            process: Process {
                pid,
                parent_pid,
                name,
                created,
                cpu_percent: 0.0,
                working_set: u64_at(144),
                private_bytes: u64_at(200),
                io_bytes_per_sec: 0.0,
                threads: u32_at(4),
                handles: u32_at(96),
            },
            counters: Counters {
                created,
                cpu: u64_at(40).saturating_add(u64_at(48)),
                io: u64_at(232)
                    .saturating_add(u64_at(240))
                    .saturating_add(u64_at(248)),
                seen: 0,
            },
        });
        if next == 0 {
            break;
        }
        offset = offset.checked_add(next).ok_or_else(invalid)?;
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter(created: u64, cpu: u64, io: u64) -> Counters {
        Counters {
            created,
            cpu,
            io,
            seen: 0,
        }
    }

    #[test]
    fn cpu_is_normalized_across_all_processors_and_io_uses_elapsed_time() {
        let (cpu, io) = rates(
            Some(counter(1, 100, 1_000)),
            counter(1, 10_000_100, 5_000),
            2.0,
            2.0 * TICKS_PER_SECOND * 8.0,
        );
        assert!((cpu - 6.25).abs() < 0.0001);
        assert_eq!(io, 2_000.0);
    }

    #[test]
    fn reused_pids_and_first_observation_have_no_inherited_rate() {
        assert_eq!(
            rates(
                Some(counter(1, 1, 1)),
                counter(2, 1_000, 1_000),
                1.0,
                TICKS_PER_SECOND
            ),
            (0.0, 0.0)
        );
        assert_eq!(
            rates(None, counter(2, 1_000, 1_000), 1.0, TICKS_PER_SECOND),
            (0.0, 0.0)
        );
    }

    #[test]
    fn counter_regression_does_not_underflow() {
        assert_eq!(
            rates(
                Some(counter(1, 2_000, 2_000)),
                counter(1, 10, 10),
                1.0,
                TICKS_PER_SECOND
            ),
            (0.0, 0.0)
        );
        assert_eq!(
            rates(Some(counter(1, 0, 0)), counter(1, 10, 10), 0.0, 0.0),
            (0.0, 0.0)
        );
    }

    #[test]
    fn system_kernel_time_includes_idle_time() {
        assert_eq!(
            system_cpu_percent(
                SystemTimes {
                    idle: 100,
                    total: 200
                },
                SystemTimes {
                    idle: 175,
                    total: 300
                }
            ),
            25.0
        );
        assert_eq!(
            system_cpu_percent(
                SystemTimes {
                    idle: 100,
                    total: 200
                },
                SystemTimes {
                    idle: 100,
                    total: 200
                }
            ),
            0.0
        );
    }

    fn record(pid: u64, name: &[u16]) -> Vec<u8> {
        let mut bytes = vec![0; HEADER_BYTES + name.len() * 2];
        bytes[80..88].copy_from_slice(&pid.to_le_bytes());
        bytes[56..58].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        bytes[58..60].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
        let pointer = bytes.as_ptr() as u64 + HEADER_BYTES as u64;
        bytes[64..72].copy_from_slice(&pointer.to_le_bytes());
        for (target, value) in bytes[HEADER_BYTES..]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(name)
        {
            target.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn parses_native_fields_and_utf16_without_dereferencing_native_pointer() {
        let mut bytes = record(123, &"테스트.exe".encode_utf16().collect::<Vec<_>>());
        bytes[144..152].copy_from_slice(&123_456u64.to_le_bytes());
        bytes[200..208].copy_from_slice(&654_321u64.to_le_bytes());
        let result = parse_processes(&bytes, 0).unwrap();
        assert_eq!(result[0].process.pid, 123);
        assert_eq!(result[0].process.name, "테스트.exe");
        assert_eq!(result[0].process.working_set, 123_456);
        assert_eq!(result[0].process.private_bytes, 654_321);
    }

    #[test]
    fn rejects_truncated_header_and_invalid_forward_offsets() {
        assert!(parse_processes(&[0; HEADER_BYTES - 1], 0).is_err());
        for offset in [1u32, 8, 255, 257, u32::MAX] {
            let mut bytes = record(123, &[]);
            bytes[..4].copy_from_slice(&offset.to_le_bytes());
            assert!(parse_processes(&bytes, 0).is_err());
        }
        let mut bytes = record(123, &[]);
        bytes[..4].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
        assert!(parse_processes(&bytes, 0).is_err());
    }

    #[test]
    fn rejects_invalid_utf16_lengths_and_out_of_buffer_pointers() {
        let mut bytes = record(123, &[65]);
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        assert!(parse_processes(&bytes, 0).is_err());
        bytes[56..58].copy_from_slice(&2u16.to_le_bytes());
        for pointer in [0u64, 1, u64::MAX, bytes.as_ptr() as u64] {
            bytes[64..72].copy_from_slice(&pointer.to_le_bytes());
            assert!(parse_processes(&bytes, 0).is_err());
        }
    }

    #[test]
    fn parses_forward_chain_and_rejects_names_in_another_record() {
        let mut bytes = vec![0u8; HEADER_BYTES * 2];
        bytes[..4].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
        bytes[80..88].copy_from_slice(&4u64.to_le_bytes());
        bytes[HEADER_BYTES + 80..HEADER_BYTES + 88].copy_from_slice(&8u64.to_le_bytes());
        let result = parse_processes(&bytes, 0).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].process.name, "System");
        assert_eq!(result[1].process.pid, 8);
        let pointer = bytes.as_ptr() as u64 + HEADER_BYTES as u64;
        bytes[64..72].copy_from_slice(&pointer.to_le_bytes());
        bytes[56..58].copy_from_slice(&2u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&2u16.to_le_bytes());
        assert!(parse_processes(&bytes, 0).is_err());
    }

    #[test]
    fn live_native_snapshot_contains_this_process_and_physical_memory() {
        let mut sampler = Sampler::new().unwrap();
        let first = sampler.sample().unwrap();
        let current = first
            .processes
            .iter()
            .find(|process| process.pid == std::process::id())
            .unwrap();
        assert!(current.created > 0);
        assert!(current.working_set > 0);
        assert!(current.threads > 0);
        assert_eq!(current.cpu_percent, 0.0);
        assert!(first.memory_total > 0);
        assert!(first.memory_used <= first.memory_total);
        let second = sampler.sample().unwrap();
        assert!(second.cpu_percent.is_finite());
        assert!(
            second
                .processes
                .iter()
                .all(|process| process.cpu_percent.is_finite()
                    && process.io_bytes_per_sec.is_finite())
        );
    }
}
