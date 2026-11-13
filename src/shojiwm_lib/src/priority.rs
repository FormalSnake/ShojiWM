//! The CPU priority of the thread that runs the event loop.
//!
//! Everything a frame needs happens on that thread, so under a nice-0 build
//! each of its wakeups (input, a vblank, the commit deadline timer, a render
//! fence) queues behind the build's jobs. SCHED_RR at the lowest RT priority
//! takes it out of that queue, as Hyprland and KWin ask for; scx_bpfland
//! gives a lower nice value almost no shorter wakeups, so nice is only the
//! fallback when RLIMIT_RTPRIO allows no RT class. Both limits let an
//! unprivileged process do this without CAP_SYS_NICE.
//! SCHED_FLAG_RESET_ON_FORK keeps it to this thread: threads and processes
//! started from it begin at SCHED_OTHER nice 0, so a terminal spawned by a
//! bind does not hand a build the compositor's priority.

use tracing::{info, warn};

/// The RT priority asked for, when RLIMIT_RTPRIO allows it.
const RT_PRIORITY: u32 = 1;
/// The nice value asked for otherwise, when RLIMIT_NICE allows it.
const NICE: i32 = -10;

const SCHED_OTHER: u32 = 0;
const SCHED_RR: u32 = 2;
const SCHED_FLAG_RESET_ON_FORK: u64 = 0x01;

/// `struct sched_attr`, SCHED_ATTR_SIZE_VER0.
#[repr(C)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
}

/// The nice value to run at: `NICE`, or the lowest RLIMIT_NICE allows
/// (20 - its soft limit), never above `current`. `None` when that is no
/// lower than `current`.
fn target_nice(current: i32, rlimit_nice: u64) -> Option<i32> {
    let floor = 20 - rlimit_nice.min(40) as i32;
    let target = NICE.max(floor);
    (target < current).then_some(target)
}

fn set(policy: u32, priority: u32, nice: i32) -> std::io::Result<()> {
    let attr = SchedAttr {
        size: std::mem::size_of::<SchedAttr>() as u32,
        sched_policy: policy,
        sched_flags: SCHED_FLAG_RESET_ON_FORK,
        sched_nice: nice,
        sched_priority: priority,
        sched_runtime: 0,
        sched_deadline: 0,
        sched_period: 0,
    };
    // SAFETY: sched_setattr on the calling thread with a valid attr.
    let status = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &attr as *const SchedAttr, 0) };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn limit(resource: libc::__rlimit_resource_t) -> std::io::Result<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit with a valid out pointer.
    if unsafe { libc::getrlimit(resource, &mut limit) } == 0 {
        Ok(limit.rlim_cur as u64)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Puts the calling thread on SCHED_RR when RLIMIT_RTPRIO allows it, or
/// lowers its nice value as far as `NICE` and RLIMIT_NICE allow, marked
/// reset-on-fork either way.
pub fn raise_event_loop_thread() {
    match limit(libc::RLIMIT_RTPRIO) {
        Ok(rtprio) if rtprio >= RT_PRIORITY as u64 => match set(SCHED_RR, RT_PRIORITY, 0) {
            Ok(()) => {
                info!(priority = RT_PRIORITY, "event loop thread runs SCHED_RR");
                return;
            }
            Err(error) => warn!(%error, "could not put the event loop thread on SCHED_RR"),
        },
        Ok(_) => {}
        Err(error) => warn!(%error, "could not read RLIMIT_RTPRIO"),
    }
    // SAFETY: getpriority on the calling thread; errno is cleared first since
    // -1 is a valid nice value.
    let current = unsafe {
        *libc::__errno_location() = 0;
        libc::getpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t)
    };
    if current == -1 && std::io::Error::last_os_error().raw_os_error() != Some(0) {
        warn!(
            error = %std::io::Error::last_os_error(),
            "could not read the event loop thread's nice value"
        );
        return;
    }
    let rlimit_nice = match limit(libc::RLIMIT_NICE) {
        Ok(value) => value,
        Err(error) => {
            warn!(%error, "could not read RLIMIT_NICE");
            return;
        }
    };
    let Some(nice) = target_nice(current, rlimit_nice) else {
        info!(
            nice = current,
            rlimit_nice,
            "event loop thread keeps its priority: RLIMIT_RTPRIO and RLIMIT_NICE allow nothing higher"
        );
        return;
    };
    match set(SCHED_OTHER, 0, nice) {
        Ok(()) => info!(
            nice,
            previous = current,
            "lowered the event loop thread's nice value"
        ),
        Err(error) => warn!(nice, %error, "could not lower the event loop thread's nice value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Soft rlimits are per process, so the tests that set them take turns.
    static LIMITS: Mutex<()> = Mutex::new(());

    fn attr() -> SchedAttr {
        // SAFETY: SchedAttr is plain integers, and sched_getattr fills it for the calling thread.
        unsafe {
            let mut attr: SchedAttr = std::mem::zeroed();
            let status = libc::syscall(
                libc::SYS_sched_getattr,
                0,
                &mut attr as *mut SchedAttr,
                std::mem::size_of::<SchedAttr>() as u32,
                0,
            );
            assert_eq!(status, 0, "{}", std::io::Error::last_os_error());
            attr
        }
    }

    fn set_soft_limit(resource: libc::__rlimit_resource_t, value: u64) -> u64 {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit and setrlimit with valid pointers.
        unsafe {
            assert_eq!(libc::getrlimit(resource, &mut limit), 0);
            let previous = limit.rlim_cur as u64;
            limit.rlim_cur = value.min(limit.rlim_max as u64) as libc::rlim_t;
            assert_eq!(libc::setrlimit(resource, &limit), 0);
            previous
        }
    }

    /// Raises a fresh thread with these soft limits and returns its sched_attr and that of a
    /// thread it spawns afterwards.
    fn raise_with(rtprio: u64, nice: u64) -> (SchedAttr, SchedAttr) {
        let _turn = LIMITS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let rtprio = set_soft_limit(libc::RLIMIT_RTPRIO, rtprio);
        let nice = set_soft_limit(libc::RLIMIT_NICE, nice);
        let attrs = std::thread::spawn(|| {
            raise_event_loop_thread();
            (attr(), std::thread::spawn(attr).join().unwrap())
        })
        .join()
        .unwrap();
        set_soft_limit(libc::RLIMIT_RTPRIO, rtprio);
        set_soft_limit(libc::RLIMIT_NICE, nice);
        attrs
    }

    fn hard_limit(resource: libc::__rlimit_resource_t) -> u64 {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit with a valid out pointer.
        assert_eq!(unsafe { libc::getrlimit(resource, &mut limit) }, 0);
        limit.rlim_max as u64
    }

    #[test]
    fn without_either_limit_the_thread_is_left_as_it_was() {
        let before = std::thread::spawn(attr).join().unwrap();
        let (raised, child) = raise_with(0, 0);
        assert_eq!(raised.sched_policy, SCHED_OTHER);
        assert_eq!(raised.sched_nice, before.sched_nice);
        assert_eq!(raised.sched_flags & SCHED_FLAG_RESET_ON_FORK, 0);
        assert_eq!(child.sched_policy, SCHED_OTHER);
    }

    #[test]
    #[ignore = "needs a hard RLIMIT_RTPRIO of 1: prlimit --rtprio=1:1"]
    fn rtprio_puts_only_this_thread_on_sched_rr() {
        assert!(hard_limit(libc::RLIMIT_RTPRIO) >= 1);
        let (raised, child) = raise_with(1, 0);
        assert_eq!(raised.sched_policy, SCHED_RR);
        assert_eq!(raised.sched_priority, RT_PRIORITY);
        assert_ne!(raised.sched_flags & SCHED_FLAG_RESET_ON_FORK, 0);
        assert_eq!(
            (child.sched_policy, child.sched_priority, child.sched_nice),
            (SCHED_OTHER, 0, 0)
        );
        assert_eq!(child.sched_flags & SCHED_FLAG_RESET_ON_FORK, 0);
    }

    #[test]
    #[ignore = "needs a hard RLIMIT_NICE of 30: prlimit --nice=30:30"]
    fn without_rtprio_the_thread_runs_at_nice_minus_ten() {
        assert!(hard_limit(libc::RLIMIT_NICE) >= 30);
        let (raised, child) = raise_with(0, 30);
        assert_eq!(
            (raised.sched_policy, raised.sched_nice),
            (SCHED_OTHER, NICE)
        );
        assert_ne!(raised.sched_flags & SCHED_FLAG_RESET_ON_FORK, 0);
        assert_eq!((child.sched_policy, child.sched_nice), (SCHED_OTHER, 0));
    }

    #[test]
    fn asks_for_minus_ten_when_the_limit_allows_it() {
        assert_eq!(target_nice(0, 30), Some(-10));
        assert_eq!(target_nice(0, 40), Some(-10));
    }

    #[test]
    fn stops_at_what_the_limit_allows() {
        assert_eq!(target_nice(0, 25), Some(-5));
        assert_eq!(target_nice(0, 20), None);
        assert_eq!(target_nice(0, 0), None);
        assert_eq!(target_nice(5, 20), Some(0));
        assert_eq!(target_nice(5, 18), Some(2));
    }

    #[test]
    fn never_raises_a_nice_value_already_lower() {
        assert_eq!(target_nice(-12, 30), None);
        assert_eq!(target_nice(-10, 30), None);
        assert_eq!(target_nice(2, 18), None);
    }
}
