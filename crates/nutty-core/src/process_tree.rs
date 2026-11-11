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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{
        os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
        time::Duration,
    };
    use windows_sys::Win32::{
        Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };

    #[tokio::test]
    async fn cmd_adapter_starts_without_console_and_job_close_reaps_descendants() -> Result<()> {
        let root = tempfile::tempdir()?;
        let directory = root.path().join("Agent 中文 directory");
        std::fs::create_dir(&directory)?;
        let marker = directory.join("child.pid");
        let script = directory.join("adapter.py");
        std::fs::write(
            &script,
            r#"import subprocess, sys, time
subprocess.Popen([sys.executable, '-c', "import os,pathlib,time; pathlib.Path('child.pid').write_text(str(os.getpid())); time.sleep(60)"])
time.sleep(60)
"#,
        )?;
        let shim = directory.join("agent.cmd");
        std::fs::write(&shim, "@echo off\r\npython \"%~dp0adapter.py\"\r\n")?;
        let process = crate::integration_config::Process {
            program: shim,
            args: Vec::new(),
            directory,
            environment: false,
        };
        let mut command = process.command(uuid::Uuid::new_v4()).await?;
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        let (mut child, tree) = ProcessTree::spawn(&mut command)?;
        let pid = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&marker).await
                    && let Ok(pid) = text.parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        // SAFETY: Open a synchronization-only handle to the fixture's known child.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        anyhow::ensure!(!handle.is_null(), "fixture child must be alive");
        // SAFETY: The successful OpenProcess call returns an owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        // SAFETY: The handle stays live and has the synchronization right.
        assert_eq!(
            unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) },
            WAIT_TIMEOUT
        );
        drop(tree);
        tokio::time::timeout(Duration::from_secs(3), child.wait()).await??;
        // SAFETY: The retained handle refers to the same process even after exit.
        assert_eq!(
            unsafe { WaitForSingleObject(handle.as_raw_handle(), 3000) },
            WAIT_OBJECT_0
        );
        Ok(())
    }
}
