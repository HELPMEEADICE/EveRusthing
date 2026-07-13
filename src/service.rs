use std::ffi::c_void;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING,
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_EXISTS, ERROR_SERVICE_NOT_ACTIVE, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_NOWAIT, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    SetNamedPipeHandleState, WaitNamedPipeW,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, RegisterServiceCtrlHandlerW, SC_HANDLE, SC_MANAGER_ALL_ACCESS,
    SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONTROL_STOP,
    SERVICE_ERROR_NORMAL, SERVICE_RUNNING, SERVICE_START, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_HANDLE, SERVICE_STOP, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
    StartServiceW,
};

use crate::index::SharedIndex;
use crate::model::FileRecord;
use crate::ntfs::{NtfsVolume, discover_ntfs_volumes};
use crate::service_protocol::{
    COMMAND_PING, COMMAND_SCAN_ALL, Frame, ProtocolError, REPLY_DONE, REPLY_ERROR, REPLY_PONG,
    REPLY_RECORDS, REPLY_VOLUME, VolumeReply, decode_records, decode_volume, encode_records,
    encode_volume, read_frame, records_in_pages, write_frame,
};

pub const SERVICE_NAME: &str = "EveRusthing";
pub const DEFAULT_PIPE_NAME: &str = "EveRusthing Service";
const RECORD_PAGE_SIZE: usize = 4 * 1024 * 1024;
const PIPE_BUFFER_SIZE: u32 = 64 * 1024;
const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";
const SERVICE_NAME_WIDE: [u16; 12] = [69, 118, 101, 82, 117, 115, 116, 104, 105, 110, 103, 0];

static SERVICE_STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static SERVICE_STATUS_HANDLE_VALUE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static SERVICE_PIPE_NAME: OnceLock<String> = OnceLock::new();

#[derive(Debug)]
pub struct ServiceError {
    operation: &'static str,
    code: u32,
    detail: Option<String>,
}

impl ServiceError {
    fn windows(operation: &'static str) -> Self {
        Self {
            operation,
            code: unsafe { GetLastError() },
            detail: None,
        }
    }

    fn detail(operation: &'static str, detail: impl Into<String>) -> Self {
        Self {
            operation,
            code: 0,
            detail: Some(detail.into()),
        }
    }
}

impl Display for ServiceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if let Some(detail) = &self.detail {
            write!(formatter, "{}: {detail}", self.operation)
        } else {
            write!(
                formatter,
                "{} failed with Windows error {}",
                self.operation, self.code
            )
        }
    }
}

impl std::error::Error for ServiceError {}

#[derive(Debug, Default)]
pub struct ServiceScan {
    pub records: Vec<FileRecord>,
    pub volumes: Vec<VolumeReply>,
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl Read for Handle {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        for _ in 0..100 {
            let mut read = 0;
            let success = unsafe {
                ReadFile(
                    self.0,
                    buffer.as_mut_ptr(),
                    buffer.len().min(u32::MAX as usize) as u32,
                    &mut read,
                    null_mut(),
                )
            };
            if success != 0 {
                return Ok(read as usize);
            }
            let error = unsafe { GetLastError() };
            if error != ERROR_NO_DATA {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for service pipe data",
        ))
    }
}

impl Write for Handle {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut written = 0;
        let success = unsafe {
            WriteFile(
                self.0,
                buffer.as_ptr(),
                buffer.len().min(u32::MAX as usize) as u32,
                &mut written,
                null_mut(),
            )
        };
        if success != 0 {
            Ok(written as usize)
        } else {
            Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if unsafe { FlushFileBuffers(self.0) } != 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ))
        }
    }
}

struct ServiceHandle(SC_HANDLE);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}

struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    fn new() -> Result<Self, ServiceError> {
        let sddl = wide_null(PIPE_SDDL);
        let mut descriptor = null_mut();
        let success = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if success == 0 {
            return Err(ServiceError::windows("create pipe security descriptor"));
        }
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

pub fn pipe_path(name: &str) -> String {
    if name.starts_with(r"\\.\pipe\") || name.starts_with(r"\\.\PIPE\") {
        name.to_owned()
    } else {
        format!(r"\\.\pipe\{name}")
    }
}

pub fn scan_local(pipe_name: &str) -> Result<ServiceScan, ServiceError> {
    let mut pipe = connect_pipe(pipe_name)?;
    write_frame(
        &mut pipe,
        &Frame {
            code: COMMAND_SCAN_ALL,
            payload: Vec::new(),
        },
    )
    .map_err(protocol_error("send service scan request"))?;

    let mut result = ServiceScan::default();
    loop {
        let frame = read_frame(&mut pipe).map_err(protocol_error("read service scan reply"))?;
        match frame.code {
            REPLY_VOLUME => result.volumes.push(
                decode_volume(&frame.payload).map_err(protocol_error("decode volume reply"))?,
            ),
            REPLY_RECORDS => result.records.extend(
                decode_records(&frame.payload).map_err(protocol_error("decode record reply"))?,
            ),
            REPLY_DONE => return Ok(result),
            REPLY_ERROR => return Err(decode_service_error(&frame.payload)),
            _ => {
                return Err(ServiceError::detail(
                    "read service scan reply",
                    "unexpected reply code",
                ));
            }
        }
    }
}

pub fn ping(pipe_name: &str) -> Result<(), ServiceError> {
    let mut pipe = connect_pipe(pipe_name)?;
    write_frame(
        &mut pipe,
        &Frame {
            code: COMMAND_PING,
            payload: Vec::new(),
        },
    )
    .map_err(protocol_error("send service ping"))?;
    let reply = read_frame(&mut pipe).map_err(protocol_error("read service ping"))?;
    if reply.code == REPLY_PONG {
        Ok(())
    } else {
        Err(ServiceError::detail(
            "read service ping",
            "unexpected reply",
        ))
    }
}

pub fn run_console_server(pipe_name: &str, once: bool) -> Result<(), ServiceError> {
    SERVICE_STOP_REQUESTED.store(false, Ordering::Release);
    serve(pipe_name, once)
}

pub fn run_service_dispatcher(pipe_name: String) -> Result<(), ServiceError> {
    let _ = SERVICE_PIPE_NAME.set(pipe_name);
    SERVICE_STOP_REQUESTED.store(false, Ordering::Release);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: SERVICE_NAME_WIDE.as_ptr() as *mut u16,
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        Err(ServiceError::windows("start service control dispatcher"))
    } else {
        Ok(())
    }
}

pub fn install(executable: &Path, pipe_name: &str) -> Result<(), ServiceError> {
    let manager = open_manager()?;
    let service_name = wide_null(SERVICE_NAME);
    let display_name = wide_null("EveRusthing Service");
    let binary_path = wide_null(&format!(
        "\"{}\" -svc -svc-pipe-name \"{}\"",
        executable.display(),
        pipe_name
    ));
    let raw = unsafe {
        CreateServiceW(
            manager.0,
            service_name.as_ptr(),
            display_name.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            binary_path.as_ptr(),
            null(),
            null_mut(),
            null(),
            null(),
            null(),
        )
    };
    let service = if raw.is_null() {
        let error = unsafe { GetLastError() };
        if error != ERROR_SERVICE_EXISTS {
            return Err(ServiceError {
                operation: "create EveRusthing service",
                code: error,
                detail: None,
            });
        }
        open_service(&manager, SERVICE_START | SERVICE_STOP | SERVICE_ALL_ACCESS)?
    } else {
        ServiceHandle(raw)
    };
    start_handle(&service)
}

pub fn start() -> Result<(), ServiceError> {
    let manager = open_manager()?;
    let service = open_service(&manager, SERVICE_START)?;
    start_handle(&service)
}

pub fn stop() -> Result<(), ServiceError> {
    let manager = open_manager()?;
    let service = open_service(&manager, SERVICE_STOP)?;
    let mut status = SERVICE_STATUS::default();
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        let error = unsafe { GetLastError() };
        if error != ERROR_SERVICE_NOT_ACTIVE {
            return Err(ServiceError {
                operation: "stop EveRusthing service",
                code: error,
                detail: None,
            });
        }
    }
    Ok(())
}

pub fn uninstall() -> Result<(), ServiceError> {
    let manager = open_manager()?;
    let service = open_service(&manager, SERVICE_STOP | 0x0001_0000)?;
    let mut status = SERVICE_STATUS::default();
    unsafe {
        ControlService(service.0, SERVICE_CONTROL_STOP, &mut status);
    }
    if unsafe { DeleteService(service.0) } == 0 {
        Err(ServiceError::windows("delete EveRusthing service"))
    } else {
        Ok(())
    }
}

fn serve(pipe_name: &str, once: bool) -> Result<(), ServiceError> {
    let security = PipeSecurity::new()?;
    let pipe_path = wide_null(&pipe_path(pipe_name));
    loop {
        if SERVICE_STOP_REQUESTED.load(Ordering::Acquire) {
            return Ok(());
        }
        let raw = unsafe {
            CreateNamedPipeW(
                pipe_path.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                1000,
                &security.attributes,
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(ServiceError::windows("create EveRusthing service pipe"));
        }
        let mut pipe = Handle(raw);
        if wait_for_client(&pipe)? {
            let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
            if unsafe { SetNamedPipeHandleState(pipe.0, &mode, null(), null()) } == 0 {
                return Err(ServiceError::windows("set service pipe mode"));
            }
            let client_result = handle_client(&mut pipe);
            if let Err(error) = &client_result {
                let _ = send_error(&mut pipe, 0, &error.to_string());
            }
            unsafe {
                DisconnectNamedPipe(pipe.0);
            }
            if once {
                return client_result;
            }
        }
    }
}

fn wait_for_client(pipe: &Handle) -> Result<bool, ServiceError> {
    loop {
        if SERVICE_STOP_REQUESTED.load(Ordering::Acquire) {
            return Ok(false);
        }
        if unsafe { ConnectNamedPipe(pipe.0, null_mut()) } != 0 {
            return Ok(true);
        }
        match unsafe { GetLastError() } {
            ERROR_PIPE_CONNECTED => return Ok(true),
            ERROR_PIPE_LISTENING | ERROR_NO_DATA => thread::sleep(Duration::from_millis(50)),
            code => {
                return Err(ServiceError {
                    operation: "connect EveRusthing service pipe",
                    code,
                    detail: None,
                });
            }
        }
    }
}

fn handle_client(pipe: &mut Handle) -> Result<(), ServiceError> {
    let request = read_frame(pipe).map_err(protocol_error("read service request"))?;
    match request.code {
        COMMAND_PING => write_frame(
            pipe,
            &Frame {
                code: REPLY_PONG,
                payload: Vec::new(),
            },
        )
        .map_err(protocol_error("write service ping")),
        COMMAND_SCAN_ALL => stream_local_index(pipe),
        _ => send_error(pipe, 87, "unknown service command"),
    }
}

fn stream_local_index(pipe: &mut Handle) -> Result<(), ServiceError> {
    let roots = discover_ntfs_volumes()
        .map_err(|error| ServiceError::detail("discover NTFS volumes", error.to_string()))?;
    for root in roots {
        let index = SharedIndex::default();
        let volume = NtfsVolume::open(&root)
            .map_err(|error| ServiceError::detail("open NTFS volume", error.to_string()))?;
        let scan = volume
            .scan_into(&index)
            .map_err(|error| ServiceError::detail("scan NTFS volume", error.to_string()))?;
        let next_usn = volume
            .catch_up(&index, scan.next_usn, scan.journal_id)
            .map_err(|error| ServiceError::detail("catch up USN journal", error.to_string()))?;
        write_frame(
            pipe,
            &Frame {
                code: REPLY_VOLUME,
                payload: encode_volume(&VolumeReply {
                    root: scan.volume.root,
                    volume_serial: scan.volume.volume_serial,
                    root_file_reference: scan.root_file_reference,
                    journal_id: scan.journal_id,
                    next_usn,
                    record_count: scan.record_count as u64,
                }),
            },
        )
        .map_err(protocol_error("write service volume reply"))?;

        let records = index.snapshot();
        for page in records_in_pages(&records, RECORD_PAGE_SIZE) {
            write_frame(
                pipe,
                &Frame {
                    code: REPLY_RECORDS,
                    payload: encode_records(page),
                },
            )
            .map_err(protocol_error("write service record reply"))?;
        }
    }
    write_frame(
        pipe,
        &Frame {
            code: REPLY_DONE,
            payload: Vec::new(),
        },
    )
    .map_err(protocol_error("write service completion"))
}

fn connect_pipe(name: &str) -> Result<Handle, ServiceError> {
    let path = wide_null(&pipe_path(name));
    unsafe {
        WaitNamedPipeW(path.as_ptr(), 5000);
    }
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(ServiceError::windows("connect to EveRusthing service"))
    } else {
        Ok(Handle(handle))
    }
}

fn send_error(pipe: &mut Handle, code: u32, message: &str) -> Result<(), ServiceError> {
    let mut payload = Vec::with_capacity(4 + message.len());
    payload.extend(code.to_le_bytes());
    payload.extend(message.as_bytes());
    write_frame(
        pipe,
        &Frame {
            code: REPLY_ERROR,
            payload,
        },
    )
    .map_err(protocol_error("write service error"))
}

fn decode_service_error(payload: &[u8]) -> ServiceError {
    let code = payload
        .get(..4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
        .unwrap_or(0);
    let detail = payload
        .get(4..)
        .map(String::from_utf8_lossy)
        .map(|value| value.into_owned());
    ServiceError {
        operation: "EveRusthing service",
        code,
        detail,
    }
}

fn protocol_error(operation: &'static str) -> impl FnOnce(ProtocolError) -> ServiceError {
    move |error| ServiceError::detail(operation, error.to_string())
}

fn open_manager() -> Result<ServiceHandle, ServiceError> {
    let handle = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_ALL_ACCESS) };
    if handle.is_null() {
        Err(ServiceError::windows("open service control manager"))
    } else {
        Ok(ServiceHandle(handle))
    }
}

fn open_service(manager: &ServiceHandle, access: u32) -> Result<ServiceHandle, ServiceError> {
    let name = wide_null(SERVICE_NAME);
    let handle = unsafe { OpenServiceW(manager.0, name.as_ptr(), access) };
    if handle.is_null() {
        Err(ServiceError::windows("open EveRusthing service"))
    } else {
        Ok(ServiceHandle(handle))
    }
}

fn start_handle(service: &ServiceHandle) -> Result<(), ServiceError> {
    if unsafe { StartServiceW(service.0, 0, null()) } == 0 {
        let error = unsafe { GetLastError() };
        if error != ERROR_SERVICE_ALREADY_RUNNING {
            return Err(ServiceError {
                operation: "start EveRusthing service",
                code: error,
                detail: None,
            });
        }
    }
    Ok(())
}

unsafe extern "system" fn service_main(_argument_count: u32, _arguments: *mut *mut u16) {
    let handle = unsafe {
        RegisterServiceCtrlHandlerW(SERVICE_NAME_WIDE.as_ptr(), Some(service_control_handler))
    };
    if handle.is_null() {
        return;
    }
    SERVICE_STATUS_HANDLE_VALUE.store(handle, Ordering::Release);
    report_service_status(SERVICE_START_PENDING, 0, 3000);
    report_service_status(SERVICE_RUNNING, SERVICE_ACCEPT_STOP, 0);

    let pipe_name = SERVICE_PIPE_NAME
        .get()
        .map(String::as_str)
        .unwrap_or(DEFAULT_PIPE_NAME);
    let result = serve(pipe_name, false);
    let exit_code = result.err().map_or(0, |error| error.code.max(1));
    report_service_status(SERVICE_STOPPED, 0, exit_code);
}

unsafe extern "system" fn service_control_handler(control: u32) {
    if control == SERVICE_CONTROL_STOP {
        SERVICE_STOP_REQUESTED.store(true, Ordering::Release);
        report_service_status(SERVICE_STOP_PENDING, 0, 3000);
    }
}

fn report_service_status(state: u32, accepted: u32, win32_exit_code: u32) {
    let handle: SERVICE_STATUS_HANDLE = SERVICE_STATUS_HANDLE_VALUE.load(Ordering::Acquire);
    if handle.is_null() {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: win32_exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING {
            3000
        } else {
            0
        },
    };
    unsafe {
        SetServiceStatus(handle, &status);
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_default_and_explicit_pipe_paths() {
        assert_eq!(
            pipe_path("EveRusthing Service"),
            r"\\.\pipe\EveRusthing Service"
        );
        assert_eq!(pipe_path(r"\\.\PIPE\custom"), r"\\.\PIPE\custom");
    }

    #[test]
    fn service_errors_decode_original_error_reply_shape() {
        let mut payload = 5u32.to_le_bytes().to_vec();
        payload.extend(b"access denied");
        let error = decode_service_error(&payload);
        assert_eq!(error.code, 5);
        assert_eq!(error.detail.as_deref(), Some("access denied"));
    }
}
