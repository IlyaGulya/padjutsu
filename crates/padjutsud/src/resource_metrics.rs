use std::ffi::CStr;
use std::collections::HashMap;
use std::mem::{self, MaybeUninit};
use std::ptr;
use std::time::Instant;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProcessSnapshot {
    user_cpu_us: u64,
    system_cpu_us: u64,
    minor_faults: u64,
    major_faults: u64,
    voluntary_context_switches: u64,
    involuntary_context_switches: u64,
    max_rss_bytes: u64,
}

impl ProcessSnapshot {
    fn capture() -> Option<Self> {
        let mut usage = MaybeUninit::<libc::rusage>::zeroed();
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        let usage = unsafe { usage.assume_init() };
        Some(Self {
            user_cpu_us: timeval_us(usage.ru_utime),
            system_cpu_us: timeval_us(usage.ru_stime),
            minor_faults: nonnegative(usage.ru_minflt),
            major_faults: nonnegative(usage.ru_majflt),
            voluntary_context_switches: nonnegative(usage.ru_nvcsw),
            involuntary_context_switches: nonnegative(usage.ru_nivcsw),
            // Darwin reports ru_maxrss in bytes (Linux reports KiB).
            max_rss_bytes: nonnegative(usage.ru_maxrss),
        })
    }

    fn delta(self, previous: Self) -> Self {
        Self {
            user_cpu_us: self.user_cpu_us.saturating_sub(previous.user_cpu_us),
            system_cpu_us: self.system_cpu_us.saturating_sub(previous.system_cpu_us),
            minor_faults: self.minor_faults.saturating_sub(previous.minor_faults),
            major_faults: self.major_faults.saturating_sub(previous.major_faults),
            voluntary_context_switches: self
                .voluntary_context_switches
                .saturating_sub(previous.voluntary_context_switches),
            involuntary_context_switches: self
                .involuntary_context_switches
                .saturating_sub(previous.involuntary_context_switches),
            max_rss_bytes: self.max_rss_bytes,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SwapUsage {
    total: u64,
    available: u64,
    used: u64,
    page_size: u32,
    encrypted: i32,
}

#[derive(Debug, Clone, Copy, Default)]
struct SystemSnapshot {
    swap_total_mb: u64,
    swap_used_mb: u64,
    vm_pressure_level: i32,
    load_1m_x100: i32,
    load_5m_x100: i32,
    load_15m_x100: i32,
    logical_cpus: i64,
}

const PROC_PIDTHREADINFO: i32 = 5;
const PROC_PIDLISTTHREADS: i32 = 6;

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcThreadInfo {
    user_time: u64,
    system_time: u64,
    cpu_usage: i32,
    policy: i32,
    run_state: i32,
    flags: i32,
    sleep_time: i32,
    current_priority: i32,
    priority: i32,
    max_priority: i32,
    name: [libc::c_char; 64],
}

impl Default for ProcThreadInfo {
    fn default() -> Self {
        Self {
            user_time: 0,
            system_time: 0,
            cpu_usage: 0,
            policy: 0,
            run_state: 0,
            flags: 0,
            sleep_time: 0,
            current_priority: 0,
            priority: 0,
            max_priority: 0,
            name: [0; 64],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadSnapshot {
    id: u64,
    name: String,
    user_time_ns: u64,
    system_time_ns: u64,
    cpu_usage_x10: i32,
    policy: i32,
    run_state: i32,
    flags: i32,
    sleep_time_s: i32,
    current_priority: i32,
    priority: i32,
    max_priority: i32,
}

impl ThreadSnapshot {
    fn cpu_delta_us(&self, previous: Option<&Self>) -> (u64, u64) {
        let previous_user = previous.map_or(0, |snapshot| snapshot.user_time_ns);
        let previous_system = previous.map_or(0, |snapshot| snapshot.system_time_ns);
        (
            self.user_time_ns.saturating_sub(previous_user) / 1_000,
            self.system_time_ns.saturating_sub(previous_system) / 1_000,
        )
    }
}

unsafe extern "C" {
    fn proc_pidinfo(
        pid: i32,
        flavor: i32,
        arg: u64,
        buffer: *mut libc::c_void,
        buffer_size: i32,
    ) -> i32;
}

fn capture_threads() -> Vec<ThreadSnapshot> {
    const MAX_THREADS: usize = 256;
    let pid = unsafe { libc::getpid() };
    let mut ids = vec![0_u64; MAX_THREADS];
    let bytes = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDLISTTHREADS,
            0,
            ids.as_mut_ptr().cast(),
            i32::try_from(ids.len() * mem::size_of::<u64>()).unwrap_or(i32::MAX),
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }
    ids.truncate(bytes as usize / mem::size_of::<u64>());
    ids.into_iter()
        .filter(|id| *id != 0)
        .filter_map(|id| {
            let mut info = ProcThreadInfo::default();
            let bytes = unsafe {
                proc_pidinfo(
                    pid,
                    PROC_PIDTHREADINFO,
                    id,
                    (&mut info as *mut ProcThreadInfo).cast(),
                    i32::try_from(mem::size_of::<ProcThreadInfo>()).ok()?,
                )
            };
            if bytes != i32::try_from(mem::size_of::<ProcThreadInfo>()).ok()? {
                return None;
            }
            let name = unsafe { CStr::from_ptr(info.name.as_ptr()) }
                .to_string_lossy()
                .replace([' ', '='], "_");
            Some(ThreadSnapshot {
                id,
                name: if name.is_empty() {
                    "unnamed".to_owned()
                } else {
                    name
                },
                user_time_ns: info.user_time,
                system_time_ns: info.system_time,
                // Darwin scales instantaneous CPU usage to 1000 == 100%.
                cpu_usage_x10: info.cpu_usage,
                policy: info.policy,
                run_state: info.run_state,
                flags: info.flags,
                sleep_time_s: info.sleep_time,
                current_priority: info.current_priority,
                priority: info.priority,
                max_priority: info.max_priority,
            })
        })
        .collect()
}

impl SystemSnapshot {
    fn capture() -> Self {
        let swap = sysctl_value::<SwapUsage>(c"vm.swapusage").unwrap_or_default();
        let pressure = sysctl_value::<i32>(c"kern.memorystatus_vm_pressure_level")
            .unwrap_or(-1);
        let mut load = [0.0_f64; 3];
        let load_count = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
        if load_count != 3 {
            load = [-1.0; 3];
        }
        Self {
            swap_total_mb: swap.total / (1024 * 1024),
            swap_used_mb: swap.used / (1024 * 1024),
            vm_pressure_level: pressure,
            load_1m_x100: (load[0] * 100.0).round() as i32,
            load_5m_x100: (load[1] * 100.0).round() as i32,
            load_15m_x100: (load[2] * 100.0).round() as i32,
            logical_cpus: unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) },
        }
    }
}

const PROC_PIDTASKINFO: i32 = 4;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TaskSnapshot {
    virtual_size_bytes: u64,
    resident_size_bytes: u64,
    total_user: u64,
    total_system: u64,
    threads_user: u64,
    threads_system: u64,
    policy: i32,
    faults: i32,
    pageins: i32,
    cow_faults: i32,
    messages_sent: i32,
    messages_received: i32,
    syscalls_mach: i32,
    syscalls_unix: i32,
    context_switches: i32,
    thread_count: i32,
    running_thread_count: i32,
    priority: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TaskDelta {
    faults: u64,
    pageins: u64,
    cow_faults: u64,
    messages_sent: u64,
    messages_received: u64,
    syscalls_mach: u64,
    syscalls_unix: u64,
    context_switches: u64,
}

impl TaskSnapshot {
    fn capture() -> Option<Self> {
        let mut snapshot = Self::default();
        let expected = i32::try_from(mem::size_of::<Self>()).ok()?;
        let bytes = unsafe {
            proc_pidinfo(
                libc::getpid(),
                PROC_PIDTASKINFO,
                0,
                (&mut snapshot as *mut Self).cast(),
                expected,
            )
        };
        (bytes == expected).then_some(snapshot)
    }

    fn delta(self, previous: Self) -> TaskDelta {
        TaskDelta {
            faults: counter_delta(self.faults, previous.faults),
            pageins: counter_delta(self.pageins, previous.pageins),
            cow_faults: counter_delta(self.cow_faults, previous.cow_faults),
            messages_sent: counter_delta(self.messages_sent, previous.messages_sent),
            messages_received: counter_delta(
                self.messages_received,
                previous.messages_received,
            ),
            syscalls_mach: counter_delta(self.syscalls_mach, previous.syscalls_mach),
            syscalls_unix: counter_delta(self.syscalls_unix, previous.syscalls_unix),
            context_switches: counter_delta(
                self.context_switches,
                previous.context_switches,
            ),
        }
    }
}

fn counter_delta(current: i32, previous: i32) -> u64 {
    nonnegative(current).saturating_sub(nonnegative(previous))
}

fn timeval_us(value: libc::timeval) -> u64 {
    nonnegative(value.tv_sec)
        .saturating_mul(1_000_000)
        .saturating_add(nonnegative(value.tv_usec))
}

fn nonnegative<T>(value: T) -> u64
where
    T: TryInto<u64>,
{
    value.try_into().unwrap_or(0)
}

fn sysctl_value<T: Copy>(name: &CStr) -> Option<T> {
    let mut value = MaybeUninit::<T>::zeroed();
    let mut size = mem::size_of::<T>();
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    (result == 0 && size == mem::size_of::<T>())
        .then(|| unsafe { value.assume_init() })
}

pub fn spawn() {
    if !padjutsu_metrics::enabled() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("resource-metrics".into())
        .stack_size(128 * 1024)
        .spawn(run);
}

fn run() {
    let interval = padjutsu_metrics::report_interval();
    let mut started_at = Instant::now();
    let mut previous = ProcessSnapshot::capture();
    let mut previous_task = TaskSnapshot::capture();
    let mut previous_threads: HashMap<u64, ThreadSnapshot> = capture_threads()
        .into_iter()
        .map(|snapshot| (snapshot.id, snapshot))
        .collect();
    loop {
        std::thread::sleep(interval);
        let now = Instant::now();
        let Some(current) = ProcessSnapshot::capture() else {
            started_at = now;
            previous = None;
            continue;
        };
        let Some(before) = previous.replace(current) else {
            started_at = now;
            continue;
        };
        let delta = current.delta(before);
        let current_task = TaskSnapshot::capture();
        let task_delta = current_task
            .zip(previous_task)
            .map_or_else(TaskDelta::default, |(current, previous)| {
                current.delta(previous)
            });
        previous_task = current_task;
        let system = SystemSnapshot::capture();
        padjutsu_metrics::metric!(
            "resource",
            "[resource-metrics] window_ms={} user_cpu_us={} system_cpu_us={} minor_faults={} major_faults={} voluntary_ctx_switches={} involuntary_ctx_switches={} max_rss_bytes={} resident_size_bytes={} virtual_size_bytes={} task_faults={} task_pageins={} task_cow_faults={} mach_messages_sent={} mach_messages_received={} mach_syscalls={} unix_syscalls={} task_ctx_switches={} thread_count={} running_thread_count={} task_policy={} task_priority={} swap_used_mb={} swap_total_mb={} vm_pressure_level={} host_load_x100={},{},{} logical_cpus={}",
            now.saturating_duration_since(started_at).as_millis(),
            delta.user_cpu_us,
            delta.system_cpu_us,
            delta.minor_faults,
            delta.major_faults,
            delta.voluntary_context_switches,
            delta.involuntary_context_switches,
            delta.max_rss_bytes,
            current_task.map_or(0, |task| task.resident_size_bytes),
            current_task.map_or(0, |task| task.virtual_size_bytes),
            task_delta.faults,
            task_delta.pageins,
            task_delta.cow_faults,
            task_delta.messages_sent,
            task_delta.messages_received,
            task_delta.syscalls_mach,
            task_delta.syscalls_unix,
            task_delta.context_switches,
            current_task.map_or(0, |task| task.thread_count),
            current_task.map_or(0, |task| task.running_thread_count),
            current_task.map_or(0, |task| task.policy),
            current_task.map_or(0, |task| task.priority),
            system.swap_used_mb,
            system.swap_total_mb,
            system.vm_pressure_level,
            system.load_1m_x100,
            system.load_5m_x100,
            system.load_15m_x100,
            system.logical_cpus,
        );
        let current_threads = capture_threads();
        for thread in &current_threads {
            let (user_cpu_us, system_cpu_us) =
                thread.cpu_delta_us(previous_threads.get(&thread.id));
            padjutsu_metrics::metric!(
                "thread-resource",
                "[thread-resource-metrics] name={} id={} user_cpu_us={} system_cpu_us={} cpu_usage_x10={} policy={} run_state={} flags=0x{:x} sleep_time_s={} current_priority={} priority={} max_priority={}",
                thread.name,
                thread.id,
                user_cpu_us,
                system_cpu_us,
                thread.cpu_usage_x10,
                thread.policy,
                thread.run_state,
                thread.flags,
                thread.sleep_time_s,
                thread.current_priority,
                thread.priority,
                thread.max_priority,
            );
        }
        previous_threads = current_threads
            .into_iter()
            .map(|snapshot| (snapshot.id, snapshot))
            .collect();
        started_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_snapshot_delta_is_interval_scoped_and_saturating() {
        let before = ProcessSnapshot {
            user_cpu_us: 100,
            system_cpu_us: 50,
            minor_faults: 10,
            major_faults: 2,
            voluntary_context_switches: 30,
            involuntary_context_switches: 20,
            max_rss_bytes: 1_000,
        };
        let after = ProcessSnapshot {
            user_cpu_us: 160,
            system_cpu_us: 40,
            minor_faults: 17,
            major_faults: 3,
            voluntary_context_switches: 35,
            involuntary_context_switches: 29,
            max_rss_bytes: 1_500,
        };

        assert_eq!(
            after.delta(before),
            ProcessSnapshot {
                user_cpu_us: 60,
                system_cpu_us: 0,
                minor_faults: 7,
                major_faults: 1,
                voluntary_context_switches: 5,
                involuntary_context_switches: 9,
                max_rss_bytes: 1_500,
            }
        );
    }

    #[test]
    fn live_resource_sources_are_readable() {
        assert!(ProcessSnapshot::capture().is_some());
        let system = SystemSnapshot::capture();
        assert!(system.swap_total_mb >= system.swap_used_mb);
        assert!(system.vm_pressure_level >= 0);
        assert!(system.logical_cpus > 0);
        assert!(TaskSnapshot::capture().is_some());
        assert!(!capture_threads().is_empty());
    }

    #[test]
    fn thread_cpu_delta_is_interval_scoped_and_converts_nanoseconds() {
        let before = ThreadSnapshot {
            id: 7,
            name: "event-loop".to_owned(),
            user_time_ns: 10_000,
            system_time_ns: 20_000,
            cpu_usage_x10: 0,
            policy: 0,
            run_state: 0,
            flags: 0,
            sleep_time_s: 0,
            current_priority: 0,
            priority: 0,
            max_priority: 0,
        };
        let after = ThreadSnapshot {
            user_time_ns: 17_500,
            system_time_ns: 26_999,
            ..before.clone()
        };
        assert_eq!(after.cpu_delta_us(Some(&before)), (7, 6));
    }
}
