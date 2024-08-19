//! blondie is a rust library to do callstack sampling of a process on windows.
//!
//! You can use [`trace_command`] to execute and sample an [`std::process::Command`].
//!
//! Or you can use [`trace_child`] to start tracing an [`std::process::Child`].
//! You can also trace an arbitrary process using [`trace_pid`].

#![allow(clippy::field_reassign_with_default)]

use crossbeam_skiplist::SkipMap;
use object::Object;
use windows::core::{GUID, PCSTR, PSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_SUCCESS, ERROR_WMI_INSTANCE_NOT_FOUND, HANDLE,
    INVALID_HANDLE_VALUE, WIN32_ERROR,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES,
};
use windows::Win32::System::Diagnostics::Debug::{
    FormatMessageA, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
};
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceA, OpenTraceA, ProcessTrace, StartTraceA, SystemTraceControlGuid,
    TraceSampledProfileIntervalInfo, TraceSetInformation, TraceStackTracingInfo, CLASSIC_EVENT_ID,
    CONTROLTRACE_HANDLE, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_FLAG_IMAGE_LOAD,
    EVENT_TRACE_FLAG_PROFILE, EVENT_TRACE_LOGFILEA, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, KERNEL_LOGGER_NAMEA, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESS_TRACE_MODE_REAL_TIME, TRACE_PROFILE_INTERVAL,
    WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::System::SystemInformation::{GetVersionExA, OSVERSIONINFOA};
use windows::Win32::System::SystemServices::SE_SYSTEM_PROFILE_NAME;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcess, OpenProcessToken, SetThreadPriority,
    WaitForSingleObject, CREATE_SUSPENDED, PROCESS_ALL_ACCESS, THREAD_PRIORITY_TIME_CRITICAL,
};

use pdb_addr2line::{pdb::PDB, ContextPdbData};

use std::ffi::OsString;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr::{addr_of, addr_of_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Weak};

/// map[array_of_stacktrace_addrs] = sample_count
type StackMap = rustc_hash::FxHashMap<[u64; MAX_STACK_DEPTH], u64>;

struct ProcessTraceContext {
    /// SAFETY: must be a valid process handle
    target_process_handle: HANDLE,
    stack_counts_hashmap: StackMap,
    /// SAFETY: must be the id of the process
    target_proc_pid: u32,
    show_kernel_samples: bool,

    /// (image_path, image_base, image_size)
    image_paths: Vec<(OsString, u64, u64)>,
}
impl ProcessTraceContext {
    /// This takes ownership of the handle.
    /// SAFETY:
    ///  - `target_process_handle` must be a valid process handle.
    ///  - `target_proc_id must` be the id of the process.
    unsafe fn new(
        target_process_handle: HANDLE,
        target_proc_pid: u32,
        kernel_stacks: bool,
    ) -> Self {
        Self {
            target_process_handle,
            stack_counts_hashmap: Default::default(),
            target_proc_pid,
            show_kernel_samples: std::env::var("BLONDIE_KERNEL")
                .map(|value| {
                    let upper = value.to_uppercase();
                    ["Y", "YES", "TRUE"].iter().any(|truthy| &upper == truthy)
                })
                .unwrap_or(kernel_stacks),
            image_paths: Vec::with_capacity(1024),
        }
    }
}
impl Drop for ProcessTraceContext {
    fn drop(&mut self) {
        // SAFETY: ProcessTraceContext invariants ensure these are valid
        unsafe {
            let ret = CloseHandle(self.target_process_handle);
            if ret.0 == 0 {
                panic!(
                    "ProcessTraceContext::CloseHandle error:{:?}",
                    get_last_error("")
                );
            }
        }
    }
}

struct TraceContext {
    trace_running: AtomicBool,
    process_contexts: SkipMap<u32, ProcessContext>,
}
fn get_global_trace_context() -> Arc<TraceContext> {
    static GLOBAL_TRACE_CONTEXT: Mutex<Weak<TraceContext>> = Mutex::new(Weak::new());
    let locked = GLOBAL_TRACE_CONTEXT.lock().unwrap();

    if let Some(arc) = locked.upgrade() {
        arc.image_paths_by_proc_id.insert(key, value)
    }
}

// msdn says 192 but I got some that were bigger
//const MAX_STACK_DEPTH: usize = 192;
const MAX_STACK_DEPTH: usize = 200;

#[derive(Debug)]
pub enum Error {
    /// Blondie requires administrator privileges
    NotAnAdmin,
    /// Error writing to the provided Writer
    Write(std::io::Error),
    /// Error spawning a suspended process
    SpawnErr(std::io::Error),
    /// Error waiting for child, abandoned
    WaitOnChildErrAbandoned,
    /// Error waiting for child, timed out
    WaitOnChildErrTimeout,
    /// A call to a windows API function returned an error and we didn't know how to handle it
    Other(WIN32_ERROR, String, &'static str),
    /// We require Windows 7 or greater
    UnsupportedOsVersion,
    /// This should never happen
    UnknownError,
}
type Result<T> = std::result::Result<T, Error>;
impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Write(err)
    }
}

fn get_last_error(extra: &'static str) -> Error {
    const BUF_LEN: usize = 1024;
    let mut buf = [0u8; BUF_LEN];
    let code = unsafe { GetLastError() };
    let chars_written = unsafe {
        FormatMessageA(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            None,
            code.0,
            0,
            PSTR(buf.as_mut_ptr()),
            BUF_LEN as u32,
            None,
        )
    };
    assert!(chars_written != 0);
    let code_str = unsafe {
        std::ffi::CStr::from_ptr(buf.as_ptr().cast())
            .to_str()
            .unwrap_or("Invalid utf8 in error")
    };
    Error::Other(code, code_str.to_string(), extra)
}

/// A wrapper around `OpenProcess` that returns a handle with all access rights
unsafe fn handle_from_process_id(process_id: u32) -> Result<HANDLE> {
    match OpenProcess(PROCESS_ALL_ACCESS, false, process_id) {
        Ok(handle) => Ok(handle),
        Err(_) => Err(get_last_error("handle_from_process_id")),
    }
}

unsafe fn wait_for_process_by_handle(handle: HANDLE) -> Result<()> {
    let ret = WaitForSingleObject(handle, 0xFFFFFFFF);
    match ret.0 {
        0 => Ok(()),
        0x00000080 => Err(Error::WaitOnChildErrAbandoned),
        0x00000102 => Err(Error::WaitOnChildErrTimeout),
        _ => Err(get_last_error("wait_for_process_by_handle")),
    }
}

fn acquire_privileges() -> Result<()> {
    let mut privs = TOKEN_PRIVILEGES::default();
    privs.PrivilegeCount = 1;
    privs.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    if unsafe {
        LookupPrivilegeValueW(None, SE_SYSTEM_PROFILE_NAME, &mut privs.Privileges[0].Luid).0 == 0
    } {
        return Err(get_last_error("acquire_privileges LookupPrivilegeValueA"));
    }
    let mut pt = HANDLE::default();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut pt).0 == 0 } {
        return Err(get_last_error("OpenProcessToken"));
    }
    let adjust = unsafe { AdjustTokenPrivileges(pt, false, Some(addr_of!(privs)), 0, None, None) };
    if adjust.0 == 0 {
        let err = Err(get_last_error("AdjustTokenPrivileges"));
        unsafe {
            CloseHandle(pt);
        }
        return err;
    }
    let ret = unsafe { CloseHandle(pt) };
    if ret.0 == 0 {
        return Err(get_last_error("acquire_privileges CloseHandle"));
    }
    let status = unsafe { GetLastError() };
    if status != ERROR_SUCCESS {
        return Err(Error::NotAnAdmin);
    }
    Ok(())
}
/// SAFETY: is_suspended must only be true if `target_process` is suspended
unsafe fn trace_from_process_id(
    target_process_id: u32,
    is_suspended: bool,
    kernel_stacks: bool,
) -> Result<TraceContext> {
    let mut winver_info = OSVERSIONINFOA::default();
    winver_info.dwOSVersionInfoSize = size_of::<OSVERSIONINFOA>() as u32;
    let ret = GetVersionExA(&mut winver_info);
    if ret.0 == 0 {
        return Err(get_last_error("TraceSetInformation interval"));
    }
    // If we're not win7 or more, return unsupported
    // https://docs.microsoft.com/en-us/windows/win32/sysinfo/operating-system-version
    if winver_info.dwMajorVersion < 6
        || (winver_info.dwMajorVersion == 6 && winver_info.dwMinorVersion == 0)
    {
        return Err(Error::UnsupportedOsVersion);
    }
    acquire_privileges()?;

    // Set the sampling interval
    // Only for Win8 or more
    if winver_info.dwMajorVersion > 6
        || (winver_info.dwMajorVersion == 6 && winver_info.dwMinorVersion >= 2)
    {
        let mut interval = TRACE_PROFILE_INTERVAL::default();
        // TODO: Parameter?
        interval.Interval = (1000000000 / 8000) / 100;
        let ret = TraceSetInformation(
            None,
            // The value is supported on Windows 8, Windows Server 2012, and later.
            TraceSampledProfileIntervalInfo,
            addr_of!(interval).cast(),
            size_of::<TRACE_PROFILE_INTERVAL>() as u32,
        );
        if ret != ERROR_SUCCESS {
            return Err(get_last_error("TraceSetInformation interval"));
        }
    }

    let mut kernel_logger_name_with_nul = KERNEL_LOGGER_NAMEA
        .as_bytes()
        .iter()
        .cloned()
        .chain(Some(0))
        .collect::<Vec<u8>>();
    // Build the trace properties, we want EVENT_TRACE_FLAG_PROFILE for the "SampledProfile" event
    // https://docs.microsoft.com/en-us/windows/win32/etw/sampledprofile
    // In https://docs.microsoft.com/en-us/windows/win32/etw/event-tracing-mof-classes that event is listed as a "kernel event"
    // And https://docs.microsoft.com/en-us/windows/win32/etw/nt-kernel-logger-constants says
    // "The NT Kernel Logger session is the only session that can accept events from kernel event providers."
    // Therefore we must use GUID SystemTraceControlGuid/KERNEL_LOGGER_NAME as the session
    // EVENT_TRACE_REAL_TIME_MODE:
    //  Events are delivered when the buffers are flushed (https://docs.microsoft.com/en-us/windows/win32/etw/logging-mode-constants)
    // We also use Image_Load events to know which dlls to load debug information from for symbol resolution
    // Which is enabled by the EVENT_TRACE_FLAG_IMAGE_LOAD flag
    const KERNEL_LOGGER_NAMEA_LEN: usize = unsafe {
        let mut ptr = KERNEL_LOGGER_NAMEA.0;
        let mut len = 0;
        while *ptr != 0 {
            len += 1;
            ptr = ptr.add(1);
        }
        len
    };
    const PROPS_SIZE: usize = size_of::<EVENT_TRACE_PROPERTIES>() + KERNEL_LOGGER_NAMEA_LEN + 1;
    #[derive(Clone)]
    #[repr(C)]
    #[allow(non_camel_case_types)]
    struct EVENT_TRACE_PROPERTIES_WITH_STRING {
        data: EVENT_TRACE_PROPERTIES,
        s: [u8; KERNEL_LOGGER_NAMEA_LEN + 1],
    }
    let mut event_trace_props = EVENT_TRACE_PROPERTIES_WITH_STRING {
        data: EVENT_TRACE_PROPERTIES::default(),
        s: [0u8; KERNEL_LOGGER_NAMEA_LEN + 1],
    };
    event_trace_props.data.EnableFlags = EVENT_TRACE_FLAG_PROFILE | EVENT_TRACE_FLAG_IMAGE_LOAD;
    event_trace_props.data.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
    event_trace_props.data.Wnode.BufferSize = PROPS_SIZE as u32;
    event_trace_props.data.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    event_trace_props.data.Wnode.ClientContext = 3;
    event_trace_props.data.Wnode.Guid = SystemTraceControlGuid;
    event_trace_props.data.BufferSize = 1024;
    let core_count = std::thread::available_parallelism()
        .unwrap_or(std::num::NonZeroUsize::new(1usize).unwrap());
    event_trace_props.data.MinimumBuffers = core_count.get() as u32 * 4;
    event_trace_props.data.MaximumBuffers = core_count.get() as u32 * 6;
    event_trace_props.data.LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    event_trace_props
        .s
        .copy_from_slice(&kernel_logger_name_with_nul[..]);

    let kernel_logger_name_with_nul_pcstr = PCSTR(kernel_logger_name_with_nul.as_ptr());
    // Stop an existing session with the kernel logger, if it exists
    // We use a copy of `event_trace_props` since ControlTrace overwrites it
    {
        let mut event_trace_props_copy = event_trace_props.clone();
        let control_stop_retcode = ControlTraceA(
            None,
            kernel_logger_name_with_nul_pcstr,
            addr_of_mut!(event_trace_props_copy) as *mut _,
            EVENT_TRACE_CONTROL_STOP,
        );
        if control_stop_retcode != ERROR_SUCCESS
            && control_stop_retcode != ERROR_WMI_INSTANCE_NOT_FOUND
        {
            return Err(get_last_error("ControlTraceA STOP"));
        }
    }

    // Start kernel trace session
    let mut trace_session_handle: CONTROLTRACE_HANDLE = Default::default();
    {
        let start_retcode = StartTraceA(
            addr_of_mut!(trace_session_handle),
            kernel_logger_name_with_nul_pcstr,
            addr_of_mut!(event_trace_props) as *mut _,
        );
        if start_retcode != ERROR_SUCCESS {
            return Err(get_last_error("StartTraceA"));
        }
    }

    // Enable stack tracing
    {
        let mut stack_event_id = CLASSIC_EVENT_ID::default();
        // GUID from https://docs.microsoft.com/en-us/windows/win32/etw/nt-kernel-logger-constants
        let perfinfo_guid = GUID {
            data1: 0xce1dbfb4,
            data2: 0x137e,
            data3: 0x4da6,
            data4: [0x87, 0xb0, 0x3f, 0x59, 0xaa, 0x10, 0x2c, 0xbc],
        };
        stack_event_id.EventGuid = perfinfo_guid;
        stack_event_id.Type = 46; // Sampled profile event
        let enable_stacks_retcode = TraceSetInformation(
            trace_session_handle,
            TraceStackTracingInfo,
            addr_of!(stack_event_id).cast(),
            size_of::<CLASSIC_EVENT_ID>() as u32,
        );
        if enable_stacks_retcode != ERROR_SUCCESS {
            return Err(get_last_error("TraceSetInformation stackwalk"));
        }
    }

    let target_proc_handle = handle_from_process_id(target_process_id)?;
    let mut context_raw: *const TraceContext = Box::into_raw(Box::new(TraceContext::new(
        target_proc_handle,
        target_process_id,
        kernel_stacks,
    )?));

    let mut log = EVENT_TRACE_LOGFILEA::default();
    log.LoggerName = PSTR(kernel_logger_name_with_nul.as_mut_ptr());
    log.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME
        | PROCESS_TRACE_MODE_EVENT_RECORD
        | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
    log.Context = context_raw.cast_mut().cast();

    unsafe extern "system" fn event_record_callback(record: *mut EVENT_RECORD) {
        let provider_guid_data1 = (*record).EventHeader.ProviderId.data1;
        let event_opcode = (*record).EventHeader.EventDescriptor.Opcode;
        let context = &*(*record).UserContext.cast_const().cast::<TraceContext>();
        context.trace_running.store(true, Ordering::Relaxed);

        const EVENT_TRACE_TYPE_LOAD: u8 = 10;
        if event_opcode == EVENT_TRACE_TYPE_LOAD {
            let event = (*record).UserData.cast::<ImageLoadEvent>().read_unaligned();
            if event.ProcessId != context.target_proc_pid {
                // Ignore dlls for other processes
                return;
            }
            let filename_p = (*record)
                .UserData
                .cast::<ImageLoadEvent>()
                .offset(1)
                .cast::<u16>();
            let filename_os_string = OsString::from_wide(std::slice::from_raw_parts(
                filename_p,
                ((*record).UserDataLength as usize - size_of::<ImageLoadEvent>()) / 2,
            ));
            context.image_paths.push((
                filename_os_string,
                event.ImageBase as u64,
                event.ImageSize as u64,
            ));

            return;
        }

        // From https://docs.microsoft.com/en-us/windows/win32/etw/stackwalk
        let stackwalk_guid_data1 = 0xdef2fe46;
        let stackwalk_event_type = 32;
        if event_opcode != stackwalk_event_type || stackwalk_guid_data1 != provider_guid_data1 {
            // Ignore events other than stackwalk or dll load
            return;
        }
        let ud_p = (*record).UserData;
        let _timestamp = ud_p.cast::<u64>().read_unaligned();
        let proc = ud_p.cast::<u32>().offset(2).read_unaligned();
        let _thread = ud_p.cast::<u32>().offset(3).read_unaligned();
        if proc != context.target_proc_pid {
            // Ignore stackwalks for other processes
            return;
        }

        let stack_depth_32 = ((*record).UserDataLength - 16) / 4;
        let stack_depth_64 = stack_depth_32 / 2;
        let stack_depth = if size_of::<usize>() == 8 {
            stack_depth_64
        } else {
            stack_depth_32
        };

        let mut tmp = vec![];
        let mut stack_addrs = if size_of::<usize>() == 8 {
            std::slice::from_raw_parts(ud_p.cast::<u64>().offset(2), stack_depth as usize)
        } else {
            tmp.extend(
                std::slice::from_raw_parts(
                    ud_p.cast::<u64>().offset(2).cast::<u32>(),
                    stack_depth as usize,
                )
                .iter()
                .map(|x| *x as u64),
            );
            &tmp
        };
        if stack_addrs.len() > MAX_STACK_DEPTH {
            stack_addrs = &stack_addrs[(stack_addrs.len() - MAX_STACK_DEPTH)..];
        }

        let mut stack = [0u64; MAX_STACK_DEPTH];
        stack[..(stack_depth as usize).min(MAX_STACK_DEPTH)].copy_from_slice(stack_addrs);

        let entry = context.stack_counts_hashmap.entry(stack);
        *entry.or_insert(0) += 1;

        const DEBUG_OUTPUT_EVENTS: bool = false;
        if DEBUG_OUTPUT_EVENTS {
            #[repr(C)]
            #[derive(Debug)]
            #[allow(non_snake_case)]
            #[allow(non_camel_case_types)]
            struct EVENT_HEADERR {
                Size: u16,
                HeaderType: u16,
                Flags: u16,
                EventProperty: u16,
                ThreadId: u32,
                ProcessId: u32,
                TimeStamp: i64,
                ProviderId: ::windows::core::GUID,
                EventDescriptor: windows::Win32::System::Diagnostics::Etw::EVENT_DESCRIPTOR,
                KernelTime: u32,
                UserTime: u32,
                ProcessorTime: u64,
                ActivityId: ::windows::core::GUID,
            }
            #[repr(C)]
            #[derive(Debug)]
            #[allow(non_snake_case)]
            #[allow(non_camel_case_types)]
            struct EVENT_RECORDD {
                EventHeader: EVENT_HEADERR,
                BufferContextAnonymousProcessorNumber: u8,
                BufferContextAnonymousAlignment: u8,
                BufferContextAnonymousProcessorIndex: u16,
                BufferContextLoggerId: u16,
                ExtendedDataCount: u16,
                UserDataLength: u16,
                ExtendedData:
                    *mut windows::Win32::System::Diagnostics::Etw::EVENT_HEADER_EXTENDED_DATA_ITEM,
                UserData: *mut ::core::ffi::c_void,
                UserContext: *mut ::core::ffi::c_void,
            }
            eprintln!(
                "record {:?} {:?} proc:{proc} thread:{_thread}",
                (*record.cast::<EVENT_RECORDD>()),
                stack
            );
        }
    }
    log.Anonymous2.EventRecordCallback = Some(event_record_callback);

    let trace_processing_handle = OpenTraceA(&mut log);
    if trace_processing_handle.0 == INVALID_HANDLE_VALUE.0 as u64 {
        return Err(get_last_error("OpenTraceA processing"));
    }

    let (sender, recvr) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // This blocks
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        ProcessTrace(&[trace_processing_handle], None, None);

        let ret = CloseTrace(trace_processing_handle);
        if ret != ERROR_SUCCESS {
            panic!("Error closing trace");
        }
        sender.send(()).unwrap();
    });

    // Wait until we know for sure the trace is running
    while !context_raw.trace_running.load(Ordering::Relaxed) {
        std::hint::spin_loop();
    }
    // Resume the suspended process
    if is_suspended {
        // TODO: Do something less gross here
        // std Command/Child do not expose the main thread handle or id, so we can't easily call ResumeThread
        // Therefore, we call the undocumented NtResumeProcess. We should probably manually call CreateProcess.
        // Now that https://github.com/rust-lang/rust/issues/96723 is merged, we could use that on nightly
        let ntdll =
            windows::Win32::System::LibraryLoader::GetModuleHandleA(PCSTR("ntdll.dll\0".as_ptr()))
                .expect("Could not find ntdll.dll");
        #[allow(non_snake_case)]
        let NtResumeProcess = windows::Win32::System::LibraryLoader::GetProcAddress(
            ntdll,
            PCSTR("NtResumeProcess\0".as_ptr()),
        )
        .expect("Could not find NtResumeProcess in ntdll.dll");
        #[allow(non_snake_case)]
        let NtResumeProcess: extern "system" fn(isize) -> i32 =
            std::mem::transmute(NtResumeProcess);
        NtResumeProcess(context_raw.target_process_handle.0);
    }

    // Wait for it to end
    wait_for_process_by_handle(target_proc_handle)?;
    // This unblocks ProcessTrace
    let ret = ControlTraceA(
        <CONTROLTRACE_HANDLE as Default>::default(),
        PCSTR(kernel_logger_name_with_nul.as_ptr()),
        addr_of_mut!(event_trace_props) as *mut _,
        EVENT_TRACE_CONTROL_STOP,
    );
    if ret != ERROR_SUCCESS {
        return Err(get_last_error("ControlTraceA STOP ProcessTrace"));
    }
    // Block until processing thread is done
    // (Safeguard to make sure we don't deallocate the context before the other thread finishes using it)
    if recvr.recv().is_err() {
        return Err(Error::UnknownError);
    }

    if context_raw.show_kernel_samples {
        let kernel_module_paths = list_kernel_modules();
        context_raw.image_paths.extend(kernel_module_paths);
    }

    Ok(unsafe { *Box::from_raw(context_raw) })
}

/// The sampled results from a process execution
pub struct CollectionResults(TraceContext);
/// Trace an existing child process based only on its process ID (pid).
/// It is recommended that you use `trace_command` instead, since it suspends the process on creation
/// and only resumes it after the trace has started, ensuring that all samples are captured.
pub fn trace_pid(process_id: u32, kernel_stacks: bool) -> Result<CollectionResults> {
    let res = unsafe { trace_from_process_id(process_id, false, kernel_stacks) };
    res.map(CollectionResults)
}
/// Trace an existing child process.
/// It is recommended that you use `trace_command` instead, since it suspends the process on creation
/// and only resumes it after the trace has started, ensuring that all samples are captured.
pub fn trace_child(process: std::process::Child, kernel_stacks: bool) -> Result<CollectionResults> {
    let res = unsafe { trace_from_process_id(process.id(), false, kernel_stacks) };
    res.map(CollectionResults)
}
/// Execute `command` and trace it, periodically collecting call stacks.
/// The trace also tracks dlls and exes loaded by the process and loads the debug info for
/// them, if it can find it. The debug info is used to resolve addresses to symbol names and
/// is unloaded on TraceContext Drop.
pub fn trace_command(
    mut command: std::process::Command,
    kernel_stacks: bool,
) -> Result<CollectionResults> {
    use std::os::windows::process::CommandExt;

    // Create the target process suspended
    // TODO: Preserve existing flags instead of stomping them
    let mut proc = command
        .creation_flags(CREATE_SUSPENDED.0)
        .spawn()
        .map_err(Error::SpawnErr)?;
    let res = unsafe { trace_from_process_id(proc.id(), true, kernel_stacks) };
    if res.is_err() {
        // Kill the suspended process if we had some kind of error
        let _ = proc.kill();
    }
    res.map(CollectionResults)
}
/// A callstack and the count of samples it was found in
///
/// You can get them using [`CollectionResults::iter_callstacks`]
pub struct CallStack<'a> {
    stack: &'a [u64; MAX_STACK_DEPTH],
    sample_count: u64,
}

/// An address from a callstack
///
/// You can get them using [`CallStack::iter_resolved_addresses`]
pub struct Address {
    /// Sample Address
    pub addr: u64,
    /// Displacement into the symbol
    pub displacement: u64,
    /// Symbol names
    pub symbol_names: Vec<String>,
    /// Imager (Exe or Dll) name
    pub image_name: Option<String>,
}
type OwnedPdb = ContextPdbData<'static, 'static, std::io::Cursor<Vec<u8>>>;
type PdbDb<'a, 'b> =
    std::collections::BTreeMap<u64, (u64, u64, OsString, pdb_addr2line::Context<'a, 'b>)>;

/// Returns Vec<(image_base, image_size, image_name, addr2line pdb context)>
fn find_pdbs(images: &[(OsString, u64, u64)]) -> Vec<(u64, u64, OsString, OwnedPdb)> {
    let mut pdb_db = Vec::with_capacity(images.len());

    fn owned_pdb(pdb_file_bytes: Vec<u8>) -> Option<OwnedPdb> {
        let pdb = PDB::open(std::io::Cursor::new(pdb_file_bytes)).ok()?;
        pdb_addr2line::ContextPdbData::try_from_pdb(pdb).ok()
    }

    // Only download symbols from symbol servers if the env var is set
    let use_symsrv = std::env::var("_NT_SYMBOL_PATH").is_ok();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    for (path, image_base, image_size) in images {
        let path_str = match path.to_str() {
            Some(x) => x,
            _ => continue,
        };
        // Convert the \Device\HardDiskVolume path to a verbatim path \\?\HardDiskVolume
        let verbatim_path_os: OsString = path_str
            .trim_end_matches('\0')
            .replacen("\\Device\\", "\\\\?\\", 1)
            .into();

        let path = PathBuf::from(verbatim_path_os);

        let image_contents = match std::fs::read(&path) {
            Ok(x) => x,
            _ => continue,
        };
        let image_name = path.file_name().unwrap();
        let pe_file = match object::File::parse(&image_contents[..]) {
            Ok(x) => x,
            _ => continue,
        };

        let (pdb_path, pdb_guid, pdb_age) = match pe_file.pdb_info() {
            Ok(Some(x)) => (x.path(), x.guid(), x.age()),
            _ => continue,
        };
        let pdb_path = match std::str::from_utf8(pdb_path) {
            Ok(x) => x,
            _ => continue,
        };
        let mut pdb_path = PathBuf::from(pdb_path);
        if pdb_path.is_relative() {
            pdb_path = path.parent().unwrap().join(pdb_path);
        }
        if pdb_path.exists() {
            let mut file = match std::fs::File::open(pdb_path) {
                Err(_) => continue,
                Ok(x) => x,
            };
            let mut file_bytes = Vec::with_capacity(0);
            if file.read_to_end(&mut file_bytes).is_err() {
                continue;
            }
            let pdb_ctx = match owned_pdb(file_bytes) {
                Some(x) => x,
                _ => continue,
            };

            pdb_db.push((*image_base, *image_size, image_name.to_owned(), pdb_ctx));
        } else if use_symsrv {
            let pdb_filename = match pdb_path.file_name() {
                Some(x) => x,
                _ => continue,
            };

            let symbol_cache =
                symsrv::SymbolCache::new(symsrv::get_symbol_path_from_environment(""), false);

            let mut guid_string = String::new();
            use std::fmt::Write;
            for byte in pdb_guid[..4].iter().rev() {
                write!(&mut guid_string, "{byte:02X}").unwrap();
            }
            write!(&mut guid_string, "{:02X}", pdb_guid[5]).unwrap();
            write!(&mut guid_string, "{:02X}", pdb_guid[4]).unwrap();
            write!(&mut guid_string, "{:02X}", pdb_guid[7]).unwrap();
            write!(&mut guid_string, "{:02X}", pdb_guid[6]).unwrap();
            for byte in &pdb_guid[8..] {
                write!(&mut guid_string, "{byte:02X}").unwrap();
            }
            write!(&mut guid_string, "{pdb_age:X}").unwrap();
            let guid_str = std::ffi::OsStr::new(&guid_string);

            let relative_path: PathBuf = [pdb_filename, guid_str, pdb_filename].iter().collect();

            if let Ok(rt) = &rt {
                if let Ok(file_contents) = rt.block_on(symbol_cache.get_file(&relative_path)) {
                    let pdb_ctx = match owned_pdb(file_contents.to_vec()) {
                        Some(x) => x,
                        _ => continue,
                    };
                    pdb_db.push((*image_base, *image_size, image_name.to_owned(), pdb_ctx));
                }
            }
        }
    }
    pdb_db
}
impl<'a> CallStack<'a> {
    /// Iterate addresses in this callstack
    ///
    /// This also performs symbol resolution if possible, and tries to find the image (DLL/EXE) it comes from
    fn iter_resolved_addresses<
        F: for<'b> FnMut(u64, u64, &'b [&'b str], Option<&'b str>) -> Result<()>,
    >(
        &'a self,
        pdb_db: &'a PdbDb,
        v: &mut Vec<&'_ str>,
        mut f: F,
    ) -> Result<()> {
        fn reuse_vec<T, U>(mut v: Vec<T>) -> Vec<U> {
            // See https://users.rust-lang.org/t/pattern-how-to-reuse-a-vec-str-across-loop-iterations/61657/3
            assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<U>());
            assert_eq!(std::mem::align_of::<T>(), std::mem::align_of::<U>());
            v.clear();
            v.into_iter().map(|_| unreachable!()).collect()
        }
        let displacement = 0u64;
        let mut symbol_names_storage = reuse_vec(std::mem::take(v));
        for &addr in self.stack {
            if addr == 0 {
                *v = symbol_names_storage;
                return Ok(());
            }
            let mut symbol_names = symbol_names_storage;

            let module = pdb_db.range(..addr).next_back();
            let module = match module {
                None => {
                    f(addr, 0, &[], None)?;
                    symbol_names_storage = reuse_vec(symbol_names);
                    continue;
                }
                Some(x) => x.1,
            };
            let image_name = module.2.to_str();
            let addr_in_module = addr - module.0;

            let procedure_frames = match module.3.find_frames(addr_in_module as u32) {
                Ok(Some(x)) => x,
                _ => {
                    f(addr, 0, &[], image_name)?;
                    symbol_names_storage = reuse_vec(symbol_names);
                    continue;
                }
            };
            for frame in &procedure_frames.frames {
                symbol_names.push(frame.function.as_deref().unwrap_or("Unknown"));
            }
            f(addr, displacement, &symbol_names, image_name)?;
            symbol_names_storage = reuse_vec(symbol_names);
        }
        *v = symbol_names_storage;
        Ok(())
    }
}
impl CollectionResults {
    /// Iterate the distinct callstacks sampled in this execution
    pub fn iter_callstacks(&self) -> impl std::iter::Iterator<Item = CallStack<'_>> {
        self.0.stack_counts_hashmap.iter().map(|x| CallStack {
            stack: x.0,
            sample_count: *x.1,
        })
    }
    /// Resolve call stack symbols and write a dtrace-like sampling report to `w`
    pub fn write_dtrace<W: Write>(&self, mut w: W) -> Result<()> {
        let pdbs = find_pdbs(&self.0.image_paths);
        let pdb_db: PdbDb = pdbs
            .iter()
            .filter_map(|(a, b, c, d)| d.make_context().ok().map(|d| (*a, (*a, *b, c.clone(), d))))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut v = vec![];

        for callstack in self.iter_callstacks() {
            let mut empty_callstack = true;
            callstack.iter_resolved_addresses(
                &pdb_db,
                &mut v,
                |address, displacement, symbol_names, image_name| {
                    if !self.0.show_kernel_samples {
                        // kernel addresses have the highest bit set on windows
                        if address & (1 << 63) != 0 {
                            return Ok(());
                        }
                    }
                    let mut printed = false;
                    for symbol_name in symbol_names {
                        if let Some(image_name) = image_name {
                            printed = true;
                            if displacement != 0 {
                                writeln!(w, "\t\t{image_name}`{symbol_name}+0x{displacement:X}")?;
                            } else {
                                writeln!(w, "\t\t{image_name}`{symbol_name}")?;
                            }
                        } else {
                            // Image name not found
                            if displacement != 0 {
                                printed = true;
                                writeln!(w, "\t\t{symbol_name}+0x{displacement:X}")?;
                            } else if !symbol_name.is_empty() {
                                printed = true;
                                writeln!(w, "\t\t{symbol_name}")?;
                            }
                        }
                    }
                    if symbol_names.is_empty() || !printed {
                        // Symbol not found
                        writeln!(w, "\t\t`0x{address:X}")?;
                    }
                    empty_callstack = false;
                    Ok(())
                },
            )?;

            if !empty_callstack {
                let count = callstack.sample_count;
                write!(w, "\t\t{count}\n\n")?;
            }
        }
        Ok(())
    }
}

// https://docs.microsoft.com/en-us/windows/win32/etw/image-load
#[allow(non_snake_case)]
#[derive(Debug)]
#[repr(C)]
struct ImageLoadEvent {
    ImageBase: usize,
    ImageSize: usize,
    ProcessId: u32,
    ImageCheckSum: u32,
    TimeDateStamp: u32,
    Reserved0: u32,
    DefaultBase: usize,
    Reserved1: u32,
    Reserved2: u32,
    Reserved3: u32,
    Reserved4: u32,
}

/// Returns a sequence of (image_file_path, image_base)
fn list_kernel_modules() -> Vec<(OsString, u64, u64)> {
    // kernel module enumeration code based on http://www.rohitab.com/discuss/topic/40696-list-loaded-drivers-with-ntquerysysteminformation/
    #[link(name = "ntdll")]
    extern "system" {
        fn NtQuerySystemInformation(
            SystemInformationClass: u32,
            SystemInformation: *mut (),
            SystemInformationLength: u32,
            ReturnLength: *mut u32,
        ) -> i32;
    }

    const BUF_LEN: usize = 1024 * 1024;
    let mut out_buf = vec![0u8; BUF_LEN];
    let mut out_size = 0u32;
    // 11 = SystemModuleInformation
    let retcode = unsafe {
        NtQuerySystemInformation(
            11,
            out_buf.as_mut_ptr().cast(),
            BUF_LEN as u32,
            &mut out_size,
        )
    };
    if retcode < 0 {
        //println!("Failed to load kernel modules");
        return vec![];
    }
    let number_of_modules = unsafe { out_buf.as_ptr().cast::<u32>().read_unaligned() as usize };
    #[repr(C)]
    #[derive(Debug)]
    #[allow(non_snake_case)]
    #[allow(non_camel_case_types)]
    struct _RTL_PROCESS_MODULE_INFORMATION {
        Section: *mut std::ffi::c_void,
        MappedBase: *mut std::ffi::c_void,
        ImageBase: *mut std::ffi::c_void,
        ImageSize: u32,
        Flags: u32,
        LoadOrderIndex: u16,
        InitOrderIndex: u16,
        LoadCount: u16,
        OffsetToFileName: u16,
        FullPathName: [u8; 256],
    }
    let modules = unsafe {
        let modules_ptr = out_buf
            .as_ptr()
            .cast::<u32>()
            .offset(2)
            .cast::<_RTL_PROCESS_MODULE_INFORMATION>();
        std::slice::from_raw_parts(modules_ptr, number_of_modules)
    };

    let kernel_module_paths = modules
        .iter()
        .filter_map(|module| {
            unsafe { std::ffi::CStr::from_ptr(module.FullPathName.as_ptr().cast()) }
                .to_str()
                .ok()
                .map(|mod_str_filepath| {
                    let verbatim_path_osstring: OsString = mod_str_filepath
                        .replacen("\\SystemRoot\\", "\\\\?\\C:\\Windows\\", 1)
                        .into();
                    (
                        verbatim_path_osstring,
                        module.ImageBase as u64,
                        module.ImageSize as u64,
                    )
                })
        })
        .collect();
    kernel_module_paths
}
