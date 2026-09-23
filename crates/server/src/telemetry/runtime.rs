use opentelemetry::{KeyValue, global};
use tokio::runtime::Handle;

use super::SERVICE_NAME;

pub(super) fn observe_runtime() {
    let meter = global::meter(SERVICE_NAME);
    let runtime = Handle::current().metrics();

    let rt = runtime.clone();
    meter
        .u64_observable_gauge("rustibia.tokio.workers")
        .with_callback(move |o| o.observe(rt.num_workers() as u64, &[]))
        .build();
    let rt = runtime.clone();
    meter
        .u64_observable_gauge("rustibia.tokio.tasks.alive")
        .with_callback(move |o| o.observe(rt.num_alive_tasks() as u64, &[]))
        .build();
    let rt = runtime.clone();
    meter
        .u64_observable_gauge("rustibia.tokio.global_queue.depth")
        .with_callback(move |o| o.observe(rt.global_queue_depth() as u64, &[]))
        .build();
    meter
        .f64_observable_counter("rustibia.tokio.worker.busy_time")
        .with_unit("s")
        .with_callback(move |o| {
            for worker in 0..runtime.num_workers() {
                o.observe(
                    runtime.worker_total_busy_duration(worker).as_secs_f64(),
                    &[KeyValue::new("worker", worker as i64)],
                );
            }
        })
        .build();
    meter
        .f64_observable_counter("rustibia.process.cpu.time")
        .with_unit("s")
        .with_callback(|o| {
            if let Some(seconds) = std::fs::read_to_string("/proc/self/stat")
                .ok()
                .and_then(|stat| cpu_seconds(&stat))
            {
                o.observe(seconds, &[]);
            }
        })
        .build();
    meter
        .u64_observable_gauge("rustibia.process.memory.rss")
        .with_unit("By")
        .with_callback(|o| {
            if let Some(bytes) = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|status| rss_bytes(&status))
            {
                o.observe(bytes, &[]);
            }
        })
        .build();
}

/// `utime + stime` from `/proc/self/stat`. Both are in USER_HZ, which the Linux ABI fixes at 100;
/// they are fields 14 and 15, counted from 1, and `comm` (field 2) may itself contain spaces.
fn cpu_seconds(stat: &str) -> Option<f64> {
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some((utime + stime) as f64 / 100.0)
}

fn rss_bytes(status: &str) -> Option<u64> {
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kilobytes: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kilobytes * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_seconds_sums_utime_and_stime_past_a_comm_with_spaces() {
        let stat = "4242 (a (weird) name) S 1 4242 4242 0 -1 4194304 100 0 0 0 250 50 0 0 20 0 9 0";

        assert_eq!(cpu_seconds(stat), Some(3.0));
    }

    #[test]
    fn rss_bytes_reads_the_vmrss_line() {
        let status = "Name:\tserver\nVmPeak:\t  900000 kB\nVmRSS:\t  123456 kB\nThreads:\t12\n";

        assert_eq!(rss_bytes(status), Some(123456 * 1024));
    }
}
