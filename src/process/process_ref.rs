use std::{
    borrow::Cow,
    cmp,
    convert::TryInto,
    hash::{Hash, Hasher},
    mem::{self, MaybeUninit},
    os::windows::{
        prelude::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle},
        raw::HANDLE,
    },
    path::Path,
};

use rust_win32error::Win32Error;
use winapi::{
    shared::minwindef::{FALSE, HMODULE},
    um::{
        handleapi::DuplicateHandle,
        processthreadsapi::{GetCurrentProcess, TerminateProcess},
        psapi::{EnumProcessModulesEx, LIST_MODULES_ALL},
        winnt::DUPLICATE_SAME_ACCESS,
        wow64apiset::IsWow64Process,
    },
};

use crate::{
    utils::{ArrayOrVecSlice, UninitArrayBuf},
    ModuleHandle, Process, ProcessHandle, ProcessModule,
};

/// A struct representing a running process (including the current one).
/// This struct owns the underlying process handle.
///
/// # Note
/// The underlying handle has to have the following [privileges](https://docs.microsoft.com/en-us/windows/win32/procthread/process-security-and-access-rights):
///  - `PROCESS_CREATE_THREAD`
///  - `PROCESS_QUERY_INFORMATION`
///  - `PROCESS_VM_OPERATION`
///  - `PROCESS_VM_WRITE`
///  - `PROCESS_VM_READ`
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessRef<'a>(BorrowedHandle<'a>);

impl AsRawHandle for ProcessRef<'_> {
    fn as_raw_handle(&self) -> HANDLE {
        self.0.as_raw_handle()
    }
}

impl AsHandle for ProcessRef<'_> {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl PartialEq for ProcessRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_raw_handle() == other.as_raw_handle()
    }
}

impl Eq for ProcessRef<'_> {}

impl Hash for ProcessRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_raw_handle().hash(state)
    }
}

impl<'a> From<&'a Process> for ProcessRef<'a> {
    fn from(process: &'a Process) -> Self {
        process.get_ref()
    }
}

impl<'a> ProcessRef<'a> {
    /// Creates a new instance from a borrowed handle.
    ///
    /// # Safety
    /// The handle needs to fulfill the priviliges listed in the [struct documentation](ProcessRef).
    pub unsafe fn borrow_from_handle(handle: BorrowedHandle<'a>) -> Self {
        Self(handle)
    }

    /// Returns the pseudo handle representing the current process.
    #[must_use]
    pub fn current_handle() -> BorrowedHandle<'static> {
        // the handle is only a pseudo handle representing the current process which does not need to be closed.
        unsafe { BorrowedHandle::borrow_raw_handle(Self::raw_current_handle()) }
    }

    /// Returns the raw pseudo handle representing the current process.
    #[must_use]
    pub fn raw_current_handle() -> ProcessHandle {
        unsafe { GetCurrentProcess() }
    }

    /// Returns an instance representing the current process.
    #[must_use]
    pub fn current() -> Self {
        Self(Self::current_handle())
    }

    /// Returns whether this instance represents the current process.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.handle() == ProcessRef::raw_current_handle()
    }

    /// Returns the underlying raw process handle.
    #[must_use]
    pub fn handle(&self) -> ProcessHandle {
        self.as_raw_handle()
    }

    /// Promotes this instance to an owning [`Process`] instance.
    pub fn promote_to_owned(&self) -> Result<Process, Win32Error> {
        let raw_handle = self.as_raw_handle();
        let process = unsafe { GetCurrentProcess() };
        let mut new_handle = MaybeUninit::uninit();
        let result = unsafe {
            DuplicateHandle(
                process,
                raw_handle,
                process,
                new_handle.as_mut_ptr(),
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if result == 0 {
            return Err(Win32Error::new());
        }
        Ok(unsafe { Process::from_raw_handle(new_handle.assume_init()) })
    }

    /// Returns the handles of all the modules currently loaded in this process.
    ///
    /// # Note
    /// If the process is currently starting up and has not loaded all its modules the returned list may be incomplete.
    /// This can be worked around by repeatedly calling this method.
    pub fn get_module_handles(&self) -> Result<impl AsRef<[ModuleHandle]>, Win32Error> {
        let mut module_buf = UninitArrayBuf::<ModuleHandle, 1024>::new();
        let mut module_buf_byte_size = mem::size_of::<HMODULE>() * module_buf.len();
        let mut bytes_needed_target = MaybeUninit::uninit();
        let result = unsafe {
            EnumProcessModulesEx(
                self.handle(),
                module_buf.as_mut_ptr(),
                module_buf_byte_size.try_into().unwrap(),
                bytes_needed_target.as_mut_ptr(),
                LIST_MODULES_ALL,
            )
        };
        if result == 0 {
            return Err(Win32Error::new());
        }

        let mut bytes_needed = unsafe { bytes_needed_target.assume_init() } as usize;

        let modules = if bytes_needed <= module_buf_byte_size {
            // buffer size was sufficient
            let module_buf_len = bytes_needed / mem::size_of::<HMODULE>();
            let module_buf_init = unsafe { module_buf.assume_init_all() };
            ArrayOrVecSlice::from_array(module_buf_init, 0..module_buf_len)
        } else {
            // buffer size was not sufficient
            let mut module_buf_vec = Vec::new();

            // we loop here trying to find a buffer size that fits all handles
            // this needs to be a loop as the returned bytes_needed is only valid for the modules loaded when
            // the function run, if more modules have loaded in the meantime we need to resize the buffer again.
            // This can happen often if the process is currently starting up.
            loop {
                module_buf_byte_size = cmp::max(bytes_needed, module_buf_byte_size * 2);
                let mut module_buf_len = module_buf_byte_size / mem::size_of::<HMODULE>();
                module_buf_vec.resize_with(module_buf_len, MaybeUninit::uninit);

                bytes_needed_target = MaybeUninit::uninit();
                let result = unsafe {
                    EnumProcessModulesEx(
                        self.handle(),
                        module_buf_vec[0].as_mut_ptr(),
                        module_buf_byte_size.try_into().unwrap(),
                        bytes_needed_target.as_mut_ptr(),
                        LIST_MODULES_ALL,
                    )
                };
                if result == 0 {
                    return Err(Win32Error::new());
                }
                bytes_needed = unsafe { bytes_needed_target.assume_init() } as usize;

                if bytes_needed <= module_buf_byte_size {
                    module_buf_len = bytes_needed / mem::size_of::<HMODULE>();
                    let module_buf_vec = unsafe {
                        mem::transmute::<Vec<MaybeUninit<HMODULE>>, Vec<ModuleHandle>>(
                            module_buf_vec,
                        )
                    };
                    break ArrayOrVecSlice::from_vec(module_buf_vec, 0..module_buf_len);
                }
            }
        };

        Ok(modules)
    }

    /// Searches the modules in this process for one with the given name.
    /// The comparison of names is case-insensitive.
    /// If the extension is omitted, the default library extension `.dll` is appended.
    ///
    /// # Note
    /// If the process is currently starting up and has not loaded all its modules the returned list may be incomplete.
    /// This can be worked around by repeatedly calling this method.
    pub fn find_module_by_name(
        &self,
        module_name: impl AsRef<Path>,
    ) -> Result<Option<ProcessModule<'a>>, Win32Error> {
        let target_module_name = module_name.as_ref();

        // add default file extension if missing
        let target_module_name = if target_module_name.extension().is_some() {
            Cow::Owned(target_module_name.with_extension("dll").into_os_string())
        } else {
            Cow::Borrowed(target_module_name.as_os_str())
        };

        let modules = self.get_module_handles()?;

        for &module_handle in modules.as_ref() {
            let module = unsafe { ProcessModule::new(module_handle, *self) };
            let module_name = module.get_base_name()?;

            if module_name.eq_ignore_ascii_case(&target_module_name) {
                return Ok(Some(module));
            }
        }

        Ok(None)
    }

    /// Searches the modules in this process for one with the given path.
    /// The comparison of paths is case-insensitive.
    /// If the extension is omitted, the default library extension `.dll` is appended.
    ///
    /// # Note
    /// If the process is currently starting up and has not loaded all its modules the returned list may be incomplete.
    /// This can be worked around by repeatedly calling this method.
    pub fn find_module_by_path(
        &self,
        module_path: impl AsRef<Path>,
    ) -> Result<Option<ProcessModule<'a>>, Win32Error> {
        let target_module_path = module_path.as_ref();

        // add default file extension if missing
        let target_module_path = if target_module_path.extension().is_some() {
            Cow::Owned(target_module_path.with_extension("dll").into_os_string())
        } else {
            Cow::Borrowed(target_module_path.as_os_str())
        };

        let modules = self.get_module_handles()?;

        for &module_handle in modules.as_ref() {
            let module = unsafe { ProcessModule::new(module_handle, *self) };
            let module_path = module.get_path()?.into_os_string();

            if module_path.eq_ignore_ascii_case(&target_module_path) {
                return Ok(Some(module));
            }
        }

        Ok(None)
    }

    /// Returns whether this process is running under [WOW64](https://docs.microsoft.com/en-us/windows/win32/winprog64/running-32-bit-applications).
    /// This is the case for 32-bit programs running on an 64-bit platform.
    ///
    /// # Note
    /// This method returns `false` for a 32-bit process running under 32-bit Windows or 64-bit Windows 10 on ARM.
    pub fn is_wow64(&self) -> Result<bool, Win32Error> {
        let mut is_wow64 = MaybeUninit::uninit();
        let result = unsafe { IsWow64Process(self.handle(), is_wow64.as_mut_ptr()) };
        if result == 0 {
            return Err(Win32Error::new());
        }
        Ok(unsafe { is_wow64.assume_init() } != FALSE)
    }

    /// Terminates this process with exit code 1.
    pub fn kill(self) -> Result<(), Win32Error> {
        self.kill_with_exit_code(1)
    }

    /// Terminates this process with the given exit code.
    pub fn kill_with_exit_code(self, exit_code: u32) -> Result<(), Win32Error> {
        let result = unsafe { TerminateProcess(self.handle(), exit_code) };
        if result == 0 {
            return Err(Win32Error::new());
        }
        Ok(())
    }
}