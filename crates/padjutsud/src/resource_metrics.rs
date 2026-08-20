use std::ffi::CStr;
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
}

impl SystemSnapshot {
    fn capture() -> Self {
        let swap = sysctl_value::<SwapUsage>(c"vm.swapusage").unwrap_or_default();
        let pressure = sysctl_value::<i32>(c"kern.memorystatus_vm_pressure_level")
            .unwrap_or(-1);
        Self {
            swap_total_mb: swap.total / (1024 * 1024),
            swap_used_mb: swap.used / (1024 * 1024),
            vm_pressure_level: pressure,
        }
    }
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
        let system = SystemSnapshot::capture();
        padjutsu_metrics::metric!(
            "resource",
            "[resource-metrics] window_ms={} user_cpu_us={} system_cpu_us={} minor_faults={} major_faults={} voluntary_ctx_switches={} involuntary_ctx_switches={} max_rss_bytes={} swap_used_mb={} swap_total_mb={} vm_pressure_level={}",
            now.saturating_duration_since(started_at).as_millis(),
            delta.user_cpu_us,
            delta.system_cpu_us,
            delta.minor_faults,
            delta.major_faults,
            delta.voluntary_context_switches,
            delta.involuntary_context_switches,
            delta.max_rss_bytes,
            system.swap_used_mb,
            system.swap_total_mb,
            system.vm_pressure_level,
        );
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
    }
}
