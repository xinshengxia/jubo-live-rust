//! Kernel-owned launch chain, established before the browser can spawn children.
//! No PID ancestry guessing and no enumeration of unrelated browser processes.
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

/// Quote one CreateProcess argument, including trailing backslashes.
fn quote(argument: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for c in argument.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        result.push(c);
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::*,
        System::{JobObjects::*, Threading::*},
    };

    struct Handle(HANDLE);
    // Handles are exclusively owned, never inherited, and only used by one worker.
    unsafe impl Send for Handle {}
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(crate) struct OwnedBrowser {
        job: Handle,
        initial: Handle,
        pid: u32,
        executable: PathBuf,
        created: u64,
    }
    fn creation(process: HANDLE) -> Result<u64> {
        let (mut start, mut end, mut kernel, mut user) = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { GetProcessTimes(process, &mut start, &mut end, &mut kernel, &mut user) } != 0,
            "无法核实专属浏览器创建时间"
        );
        Ok((u64::from(start.dwHighDateTime) << 32) | u64::from(start.dwLowDateTime))
    }
    impl OwnedBrowser {
        pub(crate) fn spawn(executable: &Path, arguments: &[String]) -> Result<Self> {
            let executable = executable
                .canonicalize()
                .context("无法确认浏览器程序身份")?;
            let exe: Vec<u16> = executable
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            let mut command = vec![quote(&executable.to_string_lossy())];
            command.extend(arguments.iter().map(|value| quote(value)));
            let mut command: Vec<u16> = command.join(" ").encode_utf16().chain(Some(0)).collect();
            ensure!(command.len() < 32767, "浏览器启动参数过长");
            let job = Handle(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) });
            ensure!(!job.0.is_null(), "无法建立专属浏览器进程链");
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            ensure!(unsafe { SetInformationJobObject(job.0, JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32) } != 0,
                "无法设置专属浏览器退出清理策略");
            let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            ensure!(
                unsafe {
                    CreateProcessW(
                        exe.as_ptr(),
                        command.as_mut_ptr(),
                        ptr::null(),
                        ptr::null(),
                        0,
                        CREATE_SUSPENDED | CREATE_NO_WINDOW,
                        ptr::null(),
                        ptr::null(),
                        &startup,
                        &mut process,
                    )
                } != 0,
                "无法启动独立网页登录窗口"
            );
            let initial = Handle(process.hProcess);
            let thread = Handle(process.hThread);
            // Assignment happens while suspended: every subsequent descendant
            // inherits this unnamed job. Breakaway is never enabled.
            if unsafe { AssignProcessToJobObject(job.0, initial.0) } == 0 {
                unsafe {
                    TerminateProcess(initial.0, 1);
                    WaitForSingleObject(initial.0, 2000);
                }
                anyhow::bail!("无法安全关联本次登录窗口，请重新打开专属窗口");
            }
            let created = match creation(initial.0) {
                Ok(value) => value,
                Err(error) => {
                    unsafe {
                        TerminateJobObject(job.0, 1);
                    }
                    return Err(error);
                }
            };
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                unsafe {
                    TerminateJobObject(job.0, 1);
                }
                anyhow::bail!("无法恢复专属浏览器进程");
            }
            Ok(Self {
                job,
                initial,
                pid: process.dwProcessId,
                executable,
                created,
            })
        }
        pub(crate) fn id(&self) -> u32 {
            self.pid
        }
        pub(crate) fn owns(&self, pid: u32) -> Result<bool> {
            let process = Handle(unsafe {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | 0x0010_0000, 0, pid)
            });
            if process.0.is_null() {
                return Ok(false);
            }
            let mut belongs = 0;
            ensure!(
                unsafe { IsProcessInJob(process.0, self.job.0, &mut belongs) } != 0,
                "无法核实专属浏览器进程链"
            );
            if belongs == 0 || unsafe { WaitForSingleObject(process.0, 0) } != WAIT_TIMEOUT {
                return Ok(false);
            }
            if creation(process.0)? < self.created {
                return Ok(false);
            }
            let mut image = vec![0u16; 32768];
            let mut size = image.len() as u32;
            ensure!(
                unsafe { QueryFullProcessImageNameW(process.0, 0, image.as_mut_ptr(), &mut size) }
                    != 0,
                "无法确认专属浏览器程序身份"
            );
            let path = PathBuf::from(OsString::from_wide(&image[..size as usize]));
            Ok(path
                .canonicalize()
                .is_ok_and(|path| path == self.executable))
        }
        pub(crate) fn try_wait(&mut self) -> Result<Option<()>> {
            // Count all descendants, including the browser that inherited the
            // launcher's work after the initial process exited.
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                unsafe { std::mem::zeroed() };
            ensure!(
                unsafe {
                    QueryInformationJobObject(
                        self.job.0,
                        JobObjectBasicAccountingInformation,
                        (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                        std::mem::size_of_val(&accounting) as u32,
                        ptr::null_mut(),
                    )
                } != 0,
                "无法检查专属浏览器状态"
            );
            Ok((accounting.ActiveProcesses == 0).then_some(()))
        }
        pub(crate) fn kill(&mut self) -> Result<()> {
            ensure!(
                unsafe { TerminateJobObject(self.job.0, 0) } != 0,
                "无法关闭本次专属浏览器"
            );
            Ok(())
        }
        pub(crate) fn wait(&mut self) -> Result<()> {
            ensure!(
                unsafe { WaitForSingleObject(self.initial.0, 5000) } == WAIT_OBJECT_0,
                "专属浏览器进程尚未结束"
            );
            Ok(())
        }
    }
}
#[cfg(windows)]
pub(crate) use native::OwnedBrowser;

#[cfg(not(windows))]
pub(crate) struct OwnedBrowser(std::process::Child);
#[cfg(not(windows))]
impl OwnedBrowser {
    pub(crate) fn spawn(executable: &Path, arguments: &[String]) -> Result<Self> {
        Ok(Self(
            std::process::Command::new(executable)
                .args(arguments)
                .spawn()?,
        ))
    }
    pub(crate) fn id(&self) -> u32 {
        self.0.id()
    }
    pub(crate) fn owns(&self, _: u32) -> Result<bool> {
        anyhow::bail!("仅支持 Windows")
    }
    pub(crate) fn try_wait(&mut self) -> Result<Option<()>> {
        Ok(self.0.try_wait()?.map(|_| ()))
    }
    pub(crate) fn kill(&mut self) -> Result<()> {
        Ok(self.0.kill()?)
    }
    pub(crate) fn wait(&mut self) -> Result<()> {
        self.0.wait()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_arguments_preserve_spaces_quotes_and_trailing_slashes() {
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote("x\"y"), "\"x\\\"y\"");
        assert_eq!(quote("x\\"), "\"x\\\\\"");
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "explicit process integration; launches a hidden synthetic child"]
    fn job_tracks_handoff_and_rejects_unrelated_listener_owner() {
        let fixture = PathBuf::from(
            std::env::var_os("LIVE_HUB_BROWSER_FIXTURE").expect("explicit native fixture"),
        );
        let directory = tempfile::tempdir().unwrap();
        let status = directory.path().join("handoff.txt");
        let mut process =
            OwnedBrowser::spawn(&fixture, &["parent".into(), status.display().to_string()])
                .unwrap();
        process.wait().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !status.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let pid = std::fs::read_to_string(&status)
            .unwrap()
            .split(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            process.try_wait().unwrap().is_none(),
            "descendant survives initial process exit"
        );
        assert!(
            process.owns(pid).unwrap(),
            "same executable in the owned launch chain"
        );
        assert!(
            !process.owns(std::process::id()).unwrap(),
            "unrelated process rejected"
        );
        let port = std::fs::read_to_string(&status).unwrap().split(':').nth(1).unwrap().parse::<u16>().unwrap();
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        assert!(std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(200)).is_ok());
        drop(process);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(100)).is_ok()
            && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(100)).is_err(), "dropping the owned job must reap its remaining child");
    }
}
