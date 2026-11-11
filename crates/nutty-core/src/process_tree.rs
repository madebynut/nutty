//! Own an Agent process tree so cancellation also reclaims its descendants.

use anyhow::{Context as _, Result};

/// RAII process group on Unix, kill-on-close Job Object on Windows.
pub struct ProcessTree {
    #[cfg(unix)]
    pid: i32,
    #[cfg(windows)]
    _job: std::os::windows::io::OwnedHandle,
}

impl ProcessTree {
    /// Spawn a process with tree cleanup; failure never leaves the root running.
    pub fn spawn(command: &mut tokio::process::Command) -> Result<(tokio::process::Child, Self)> {
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context("starting Agent process")?;
        match Self::attach(&child) {
            Ok(tree) => Ok((child, tree)),
            Err(error) => {
                let _ = child.start_kill();
                Err(error)
            }
        }
    }

    fn attach(child: &tokio::process::Child) -> Result<Self> {
        let pid = child.id().context("Agent process exited during startup")?;
        #[cfg(unix)]
        {
            Ok(Self {
                pid: i32::try_from(pid)?,
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
            use windows_sys::Win32::System::{
                JobObjects::{
                    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                    SetInformationJobObject,
                },
                Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
            };
            // SAFETY: Null attributes/name create an unnamed, non-inheritable job.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            anyhow::ensure!(
                !job.is_null(),
                "creating Agent job: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: The non-null handle is newly owned by this function.
            let job = unsafe { OwnedHandle::from_raw_handle(job) };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: The live handle and buffer match the information class and size.
            let configured = unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            anyhow::ensure!(
                configured != 0,
                "configuring Agent job: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: Request only the rights required by AssignProcessToJobObject.
            let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
            anyhow::ensure!(
                !process.is_null(),
                "opening Agent process: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: OpenProcess returned a newly owned non-null handle.
            let process = unsafe { OwnedHandle::from_raw_handle(process) };
            // SAFETY: Both handles are live; assignment happens before protocol work.
            let assigned =
                unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) };
            anyhow::ensure!(
                assigned != 0,
                "assigning Agent job: {}",
                std::io::Error::last_os_error()
            );
            Ok(Self { _job: job })
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}
