use std::{
    io,
    process::{Child, Command},
};

pub const RESIDENT_BYTES: u64 = 256 * 1024 * 1024;

#[cfg_attr(not(windows), derive(Default))]
#[must_use = "Hold the containment guard until all decoder processes should be terminated"]
pub struct ProcessContainment {
    #[cfg(windows)]
    _job: std::os::windows::io::OwnedHandle,
}

impl Drop for ProcessContainment {
    fn drop(&mut self) {}
}

impl ProcessContainment {
    pub fn new(child: &Child) -> io::Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {
                _job: windows_job::contain(child)?,
            })
        }
        #[cfg(not(windows))]
        {
            let _child = child;
            Ok(Self {})
        }
    }
}

#[cfg(windows)]
mod windows_job {
    use super::{Child, RESIDENT_BYTES, io};
    use std::{
        ffi::c_void,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr,
    };

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x00000100;
    const JOB_OBJECT_LIMIT_JOB_MEMORY: u32 = 0x00000200;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x00002000;

    #[repr(C)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_information: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const _: () = {
        assert!(std::mem::size_of::<IoCounters>() == 48);
        assert!(std::mem::offset_of!(BasicLimitInformation, limit_flags) == 16);
        #[cfg(target_pointer_width = "64")]
        {
            assert!(std::mem::size_of::<BasicLimitInformation>() == 64);
            assert!(std::mem::size_of::<ExtendedLimitInformation>() == 144);
            assert!(std::mem::offset_of!(ExtendedLimitInformation, process_memory_limit) == 112);
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert!(std::mem::size_of::<BasicLimitInformation>() == 48);
            assert!(std::mem::size_of::<ExtendedLimitInformation>() == 112);
            assert!(std::mem::offset_of!(ExtendedLimitInformation, process_memory_limit) == 96);
        }
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            information_class: i32,
            information: *const c_void,
            information_length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        #[cfg(test)]
        fn QueryInformationJobObject(
            job: *mut c_void,
            information_class: i32,
            information: *mut c_void,
            information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    pub(super) fn contain(child: &Child) -> io::Result<OwnedHandle> {
        // A null security descriptor makes this unique, unnamed job handle
        // non-inheritable, so its last close always belongs to this guard.
        let raw_job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw_job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
        let mut limits = unsafe { std::mem::zeroed::<ExtendedLimitInformation>() };
        limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        limits.process_memory_limit = RESIDENT_BYTES as usize;
        limits.job_memory_limit = RESIDENT_BYTES as usize;
        let result = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&limits as *const ExtendedLimitInformation).cast(),
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        // Windows applies commit limits after association; allocations or child
        // processes created between spawn and this call are outside this guard.
        // Do not allow breakaway, so all later descendants remain in the job.
        let result =
            unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            process::{Command, Stdio},
            thread,
            time::{Duration, Instant},
        };

        #[test]
        #[allow(clippy::disallowed_methods)]
        fn containment_sets_commit_limits_and_terminates_on_drop() -> io::Result<()> {
            let mut child = Command::new("cmd")
                .args(["/C", "ping -n 60 127.0.0.1 >nul"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let guard = match super::super::ProcessContainment::new(&child) {
                Ok(guard) => guard,
                Err(error) => {
                    child.kill()?;
                    child.wait()?;
                    return Err(error);
                }
            };
            let mut limits = unsafe { std::mem::zeroed::<ExtendedLimitInformation>() };
            let result = unsafe {
                QueryInformationJobObject(
                    guard._job.as_raw_handle(),
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    (&mut limits as *mut ExtendedLimitInformation).cast(),
                    std::mem::size_of::<ExtendedLimitInformation>() as u32,
                    ptr::null_mut(),
                )
            };
            if result == 0 {
                let error = io::Error::last_os_error();
                drop(guard);
                child.wait()?;
                return Err(error);
            }
            drop(guard);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if child.try_wait()?.is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill()?;
                    child.wait()?;
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Closing the decoder job did not terminate its process",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(limits.process_memory_limit, RESIDENT_BYTES as usize);
            assert_eq!(limits.job_memory_limit, RESIDENT_BYTES as usize);
            assert_eq!(
                limits.basic_limit_information.limit_flags,
                JOB_OBJECT_LIMIT_PROCESS_MEMORY
                    | JOB_OBJECT_LIMIT_JOB_MEMORY
                    | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            );
            Ok(())
        }
    }
}

pub fn configure(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        use std::os::unix::process::CommandExt;
        // This runs only in a decoder worker, before exec; Darwin rejects these limits.
        unsafe {
            command.pre_exec(|| {
                let mut existing = std::mem::zeroed::<libc::rlimit>();
                if libc::getrlimit(libc::RLIMIT_AS, &mut existing) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let maximum = (512 * 1024 * 1024).min(existing.rlim_max);
                let limit = libc::rlimit {
                    rlim_cur: maximum,
                    rlim_max: maximum,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _command = command;
    }
}

pub fn memory_exceeded(child: &Child) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        // Darwin does not implement RLIMIT_AS/DATA/RSS. Sample the worker's actual
        // resident memory, while max_alloc and bounded image dimensions cap its allocations.
        let mut usage = unsafe { std::mem::zeroed::<libc::rusage_info_v2>() };
        let result = unsafe {
            libc::proc_pid_rusage(
                child.id() as i32,
                libc::RUSAGE_INFO_V2,
                (&mut usage as *mut libc::rusage_info_v2).cast(),
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::ENOENT)) {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(usage.ri_resident_size > RESIDENT_BYTES || usage.ri_phys_footprint > RESIDENT_BYTES)
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::io::AsRawHandle;
        #[repr(C)]
        struct MemoryCounters {
            size: u32,
            faults: u32,
            peak_working_set: usize,
            working_set: usize,
            peak_paged: usize,
            paged: usize,
            peak_nonpaged: usize,
            nonpaged: usize,
            pagefile: usize,
            peak_pagefile: usize,
        }
        #[link(name = "psapi")]
        unsafe extern "system" {
            fn GetProcessMemoryInfo(
                process: *mut std::ffi::c_void,
                counters: *mut MemoryCounters,
                size: u32,
            ) -> i32;
        }
        let mut counters = unsafe { std::mem::zeroed::<MemoryCounters>() };
        counters.size = std::mem::size_of::<MemoryCounters>() as u32;
        let result =
            unsafe { GetProcessMemoryInfo(child.as_raw_handle(), &mut counters, counters.size) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(counters.peak_working_set as u64 > RESIDENT_BYTES)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _child = child;
        Ok(false)
    }
}

pub fn terminate(child: &mut Child) -> io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
    }
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    child.wait()
}
