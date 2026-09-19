//! A login child joins its kill-on-close job atomically at process creation.
//! https://devblogs.microsoft.com/oldnewthing/20230209-00/?p=107812

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::{Command, ExitStatus};

use windows::Win32::Foundation::{
    HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

pub(super) struct LoginChild {
    process: OwnedHandle,
    job: OwnedHandle,
    #[cfg(test)]
    pub(super) id: u32,
}

impl LoginChild {
    pub(super) fn spawn(command: &Command) -> io::Result<Self> {
        let application = wide(command.get_program())?;
        let directory = command
            .get_current_dir()
            .map(|p| wide(p.as_os_str()))
            .transpose()?;
        let mut arguments = Vec::new();
        for arg in std::iter::once(command.get_program()).chain(command.get_args()) {
            if !arguments.is_empty() {
                arguments.push(b' ' as u16);
            }
            quote_argument(arg, &mut arguments)?;
        }
        arguments.push(0);
        let environment = environment(command)?;
        // No job handle is inheritable. Only the NUL stdio handle is passed to
        // the child, so app termination always closes the last job handle.
        let job =
            // SAFETY: a successful call transfers a unique, valid job handle.
            unsafe { owned(CreateJobObjectW(None, PCWSTR::null()).map_err(io::Error::other)?) };
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: valid owned job handle and a correctly sized, initialized limit structure.
        unsafe {
            SetInformationJobObject(
                handle(&job),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                u32::try_from(std::mem::size_of_val(&limits)).map_err(io::Error::other)?,
            )
            .map_err(io::Error::other)?;
        }
        let nul = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("NUL")?;
        let stdio = HANDLE(nul.as_raw_handle());
        // SAFETY: NUL belongs to this call and will be closed after process creation.
        unsafe {
            SetHandleInformation(stdio, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT)
                .map_err(io::Error::other)?;
        }
        let jobs = [handle(&job)];
        let handles = [stdio];
        let mut attributes = Attributes::new()?;
        // SAFETY: both arrays remain alive through CreateProcessW; each attribute is a HANDLE array.
        unsafe {
            UpdateProcThreadAttribute(
                attributes.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(jobs.as_ptr().cast()),
                std::mem::size_of_val(&jobs),
                None,
                None,
            )
            .map_err(io::Error::other)?;
            UpdateProcThreadAttribute(
                attributes.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                std::mem::size_of_val(&handles),
                None,
                None,
            )
            .map_err(io::Error::other)?;
        }
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: u32::try_from(std::mem::size_of::<STARTUPINFOEXW>())
                    .map_err(io::Error::other)?,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: stdio,
                hStdOutput: stdio,
                hStdError: stdio,
                ..Default::default()
            },
            lpAttributeList: attributes.ptr(),
        };
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: all strings are terminated UTF-16, the mutable command buffer
        // is owned, and environment/attribute storage outlives this call. The
        // job-list attribute binds the child before its first thread can run.
        unsafe {
            CreateProcessW(
                PCWSTR(application.as_ptr()),
                PWSTR(arguments.as_mut_ptr()),
                None,
                None,
                true,
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                Some(environment.as_ptr().cast()),
                directory
                    .as_ref()
                    .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
                &startup.StartupInfo,
                &mut info,
            )
            .map_err(io::Error::other)?;
            let _thread = owned(info.hThread);
            Ok(Self {
                process: owned(info.hProcess),
                job,
                #[cfg(test)]
                id: info.dwProcessId,
            })
        }
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        // SAFETY: the process handle remains owned by self.
        match unsafe { WaitForSingleObject(handle(&self.process), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => self.exit_status().map(Some),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub(super) fn kill(&mut self) -> io::Result<()> {
        // SAFETY: this private job contains only the isolated login process tree.
        unsafe { TerminateJobObject(handle(&self.job), 1).map_err(io::Error::other) }
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        // SAFETY: the process handle remains owned by self.
        if unsafe { WaitForSingleObject(handle(&self.process), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        self.exit_status()
    }

    fn exit_status(&self) -> io::Result<ExitStatus> {
        let mut code = 0;
        // SAFETY: the process handle and output pointer are valid.
        unsafe {
            GetExitCodeProcess(handle(&self.process), &mut code).map_err(io::Error::other)?;
        }
        Ok(ExitStatus::from_raw(code))
    }
}

impl Drop for LoginChild {
    fn drop(&mut self) {
        let _terminated = self.kill();
        // Give the login process a chance to release file handles before the
        // directory guard removes its home. Closing the job also covers a crash.
        // SAFETY: self still owns the valid process handle during Drop.
        unsafe {
            WaitForSingleObject(handle(&self.process), 5_000);
        }
    }
}

struct Attributes(Vec<usize>);

impl Attributes {
    fn new() -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: the first call only queries the required buffer size.
        unsafe {
            let _query = InitializeProcThreadAttributeList(
                LPPROC_THREAD_ATTRIBUTE_LIST::default(),
                2,
                0,
                &mut bytes,
            );
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut value = Self(vec![0; bytes.div_ceil(std::mem::size_of::<usize>())]);
        if let Err(error) =
            // SAFETY: the owned allocation is aligned and at least bytes long.
            unsafe { InitializeProcThreadAttributeList(value.ptr(), 2, 0, &mut bytes) }
        {
            // An uninitialized list must not be passed to DeleteProcThreadAttributeList.
            value.0.clear();
            return Err(io::Error::other(error));
        }
        Ok(value)
    }

    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.0.as_mut_ptr().cast())
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            // SAFETY: this list was initialized and has not yet been deleted.
            unsafe {
                DeleteProcThreadAttributeList(self.ptr());
            }
        }
    }
}

fn handle(value: &OwnedHandle) -> HANDLE {
    HANDLE(value.as_raw_handle())
}

unsafe fn owned(value: HANDLE) -> OwnedHandle {
    // SAFETY: caller transfers a unique valid Win32 handle from a successful API call.
    unsafe { OwnedHandle::from_raw_handle(value.0) }
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in process argument",
        ));
    }
    value.push(0);
    Ok(value)
}

fn quote_argument(value: &OsStr, output: &mut Vec<u16>) -> io::Result<()> {
    output.push(b'"' as u16);
    let mut slashes = 0;
    for code in wide(value)?.into_iter().take_while(|&code| code != 0) {
        if code == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        let count = if code == b'"' as u16 {
            slashes * 2 + 1
        } else {
            slashes
        };
        output.extend(std::iter::repeat_n(b'\\' as u16, count));
        output.push(code);
        slashes = 0;
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
    Ok(())
}

fn environment(command: &Command) -> io::Result<Vec<u16>> {
    let mut values: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (name, value) in command.get_envs() {
        values.retain(|(key, _)| {
            !key.to_string_lossy()
                .eq_ignore_ascii_case(&name.to_string_lossy())
        });
        if let Some(value) = value {
            values.push((name.to_owned(), value.to_owned()));
        }
    }
    values.sort_by_cached_key(|(key, _)| key.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (name, value) in values {
        let mut entry = name;
        entry.push("=");
        entry.push(value);
        block.extend(wide(&entry)?);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}
