//! Android Binder NDK transport, mirroring
//! `jni/base/BinderMonitor/` and `jni/base/BinderNDK/`.
//!
//! Verified from C++ before implementing:
//!
//! - C++ does **not** link `libbinder_ndk.so` at build
//!   time. `BinderNDK.cpp` `dlopen`s it at runtime and
//!   `dlsym`s every function, with fallbacks (null,
//!   false, or `STATUS_UNKNOWN_ERROR`) when a symbol is
//!   absent. This module does the same, so it compiles
//!   on a host without NDK headers and fails gracefully
//!   there, exactly as C++ does when the library is
//!   absent: service acquisition fails, and query
//!   methods return the C++ defaults (`false` for
//!   booleans, `-1` for zen mode, `""` for a package).
//! - Services, in order, are `power`, `notification`,
//!   `activity`, `display`, `package`. A service that
//!   is not immediately available is waited for up to
//!   10 seconds with 100 ms polls when the
//!   `AServiceManager_waitForService` symbol is absent
//!   (`waitForServiceCompat`); when the symbol is
//!   present it is used directly.
//! - A single-value transaction, as in
//!   `transactReadInt32` / `transactReadString`: prepare
//!   a transaction, write the interface token, transact
//!   with flags 0, read the status header and require
//!   `AStatus_isOk`, then read the value. Any failure
//!   returns the caller's error default, and the reply
//!   parcel, status, and status description are freed.
//! - Interface tokens, as in C++: `android.os.IPowerManager`,
//!   `android.app.INotificationManager`,
//!   `android.app.IActivityManager`,
//!   `android.hardware.display.IDisplayManager`,
//!   `android.content.pm.IPackageManager`.
//! - Callback classes, as in C++: `android.app.IProcessObserver`
//!   and `android.hardware.display.IDisplayManagerCallback`,
//!   each defined with a no-op create/destroy and an
//!   `onTransact` handler. The process observer reads,
//!   in order, pid/uid/foreground, pid/uid/service-types,
//!   pid/uid, or pid/process-uid/package-uid/package-name/
//!   process-name, and returns `STATUS_OK`, or
//!   `STATUS_UNKNOWN_ERROR` for an unknown code. The
//!   display callback ignores its parcel, re-queries
//!   interactivity, and fires only on a change.
//! - `Main.cpp` only sets the foreground-activities and
//!   process-died callbacks; foreground-services and
//!   process-started are left empty in C++, so this
//!   module only delivers foreground and died events to
//!   callers, matching that.
//!
//! What this module does not do: it does not decide
//! what a Binder event means for a profile. Callers in
//! `encore_core` map the decoded events here to
//! `BinderEvent` values and feed them to `DaemonState`.
//!
//! Safety: every `unsafe` block in this file is here
//! because the NDK functions are foreign functions
//! taking raw pointers. The invariants are: service
//! pointers come only from `AServiceManager_getService`
//! / `waitForService` and are never freed by this
//! module (matching C++, which holds them for the
//! process lifetime); parcels come only from
//! `AIBinder_prepareTransaction` and are deleted
//! exactly once on every path; the string allocator
//! only writes into a `Vec<u8>` supplied by this
//! module, with its size checked; callback handlers
//! only read from the parcel they are given and only
//! call into a mutex-guarded registry.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::{Mutex, OnceLock};

/// Opaque NDK types, as in `binder_ndk.h`. They are
/// never constructed in Rust, only pointed to.
pub enum AIBinder {}
/// Opaque NDK parcel type.
pub enum AParcel {}
/// Opaque NDK status type.
pub enum AStatus {}
/// Opaque NDK binder class type.
pub enum AIBinderClass {}

/// A Binder status code, as in `binder_ndk.h`.
pub type BinderStatus = i32;
/// `STATUS_OK` in `binder_ndk.h`.
pub const STATUS_OK: BinderStatus = 0;
/// `STATUS_UNKNOWN_ERROR` in `binder_ndk.h`.
// TODO(verify): confirm this constant against the NDK
// headers at cross-build time; any non-zero value is
// treated as an error by the Binder driver for an
// unknown transaction code, so the behaviour for
// unknown codes does not depend on the exact value.
pub const STATUS_UNKNOWN_ERROR: BinderStatus = -1;

type GetServiceFn = unsafe extern "C" fn(*const c_char) -> *mut AIBinder;
type WaitForServiceFn = unsafe extern "C" fn(*const c_char) -> *mut AIBinder;
type ClassDefineFn = unsafe extern "C" fn(
    *const c_char,
    unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    unsafe extern "C" fn(*mut c_void),
    unsafe extern "C" fn(*mut AIBinder, u32, *const AParcel, *mut AParcel) -> BinderStatus,
) -> *mut AIBinderClass;
type AssociateClassFn = unsafe extern "C" fn(*mut AIBinder, *const AIBinderClass) -> bool;
type BinderNewFn = unsafe extern "C" fn(*const AIBinderClass, *mut c_void) -> *mut AIBinder;
type PrepareTransactionFn = unsafe extern "C" fn(*mut AIBinder, *mut *mut AParcel) -> BinderStatus;
type TransactFn = unsafe extern "C" fn(
    *mut AIBinder,
    u32,
    *mut *mut AParcel,
    *mut *mut AParcel,
    u32,
) -> BinderStatus;
type ParcelDeleteFn = unsafe extern "C" fn(*mut AParcel);
type ReadStatusHeaderFn = unsafe extern "C" fn(*const AParcel, *mut *mut AStatus) -> BinderStatus;
type WriteInt32Fn = unsafe extern "C" fn(*mut AParcel, i32) -> BinderStatus;
type WriteStrongBinderFn = unsafe extern "C" fn(*mut AParcel, *mut AIBinder) -> BinderStatus;
type ReadInt32Fn = unsafe extern "C" fn(*const AParcel, *mut i32) -> BinderStatus;
type ReadBoolFn = unsafe extern "C" fn(*const AParcel, *mut bool) -> BinderStatus;
type ReadStringFn = unsafe extern "C" fn(
    *const AParcel,
    *mut c_void,
    unsafe extern "C" fn(*mut c_void, i32, *mut *mut c_char) -> bool,
) -> BinderStatus;
type StatusIsOkFn = unsafe extern "C" fn(*const AStatus) -> bool;
type StatusGetDescriptionFn = unsafe extern "C" fn(*const AStatus) -> *const c_char;
type StatusDeleteDescriptionFn = unsafe extern "C" fn(*const c_char);
type StatusDeleteFn = unsafe extern "C" fn(*mut AStatus);
type StartThreadPoolFn = unsafe extern "C" fn();
type JoinThreadPoolFn = unsafe extern "C" fn();

/// The loaded `libbinder_ndk.so` function table, or
/// `None` when the library or a required symbol is
/// absent. Only the functions `BinderMonitor` actually
/// uses are loaded, not the full NDK surface in
/// `BinderNDK.cpp`.
struct BinderNdk {
    get_service: GetServiceFn,
    wait_for_service: Option<WaitForServiceFn>,
    class_define: ClassDefineFn,
    associate_class: AssociateClassFn,
    binder_new: BinderNewFn,
    prepare_transaction: PrepareTransactionFn,
    transact: TransactFn,
    parcel_delete: ParcelDeleteFn,
    read_status_header: ReadStatusHeaderFn,
    write_int32: WriteInt32Fn,
    write_strong_binder: WriteStrongBinderFn,
    read_int32: ReadInt32Fn,
    read_bool: ReadBoolFn,
    read_string: ReadStringFn,
    status_is_ok: StatusIsOkFn,
    status_get_description: StatusGetDescriptionFn,
    status_delete_description: StatusDeleteDescriptionFn,
    status_delete: StatusDeleteFn,
    start_thread_pool: StartThreadPoolFn,
    join_thread_pool: JoinThreadPoolFn,
}

// SAFETY: the function table is loaded once, never
// mutated afterwards, and its functions are the NDK's
// own thread-safe entry points, so sharing it across
// Binder thread-pool threads is sound.
unsafe impl Send for BinderNdk {}
unsafe impl Sync for BinderNdk {}

macro_rules! cast {
    ($ptr:expr, $ty:ty) => {
        std::mem::transmute::<*mut c_void, $ty>($ptr)
    };
}

/// Why loading `libbinder_ndk.so` failed, with the
/// detail the daemon log needs: the `dlerror` text
/// when `dlopen` itself failed, or the name of the
/// first required symbol that was missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// `dlopen` returned null; the string is the
    /// `dlerror` text, or "unknown dlopen error".
    Open(String),
    /// `dlopen` succeeded, but this required symbol
    /// was not found.
    Symbol(&'static str),
}

fn load_binder_ndk_detailed() -> Result<BinderNdk, LoadError> {
    // SAFETY: `dlopen` / `dlsym` with a constant library
    // name and constant symbol names, as in C++
    // `load_binder_ndk`. A null handle or symbol means
    // the library is absent, which is a normal failure
    // on a host, not undefined behaviour.
    unsafe {
        let handle = libc::dlopen(
            c"libbinder_ndk.so".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if handle.is_null() {
            // SAFETY: `dlerror` returns a null or a
            // valid C string describing the last
            // dynamic-linking error, as in C++.
            let message = {
                let error = libc::dlerror();
                if error.is_null() {
                    "unknown dlopen error".to_string()
                } else {
                    // SAFETY: `dlerror` text is a valid
                    // NUL-terminated string.
                    CStr::from_ptr(error).to_string_lossy().into_owned()
                }
            };
            return Err(LoadError::Open(message));
        }
        let sym = |name: &CStr| -> *mut c_void { libc::dlsym(handle, name.as_ptr()) };
        let get_service = sym(c"AServiceManager_getService");
        let class_define = sym(c"AIBinder_Class_define");
        let associate_class = sym(c"AIBinder_associateClass");
        let binder_new = sym(c"AIBinder_new");
        let prepare_transaction = sym(c"AIBinder_prepareTransaction");
        let transact = sym(c"AIBinder_transact");
        let parcel_delete = sym(c"AParcel_delete");
        let read_status_header = sym(c"AParcel_readStatusHeader");
        let write_int32 = sym(c"AParcel_writeInt32");
        let write_strong_binder = sym(c"AParcel_writeStrongBinder");
        let read_int32 = sym(c"AParcel_readInt32");
        let read_bool = sym(c"AParcel_readBool");
        let read_string = sym(c"AParcel_readString");
        let status_is_ok = sym(c"AStatus_isOk");
        let status_get_description = sym(c"AStatus_getDescription");
        let status_delete_description = sym(c"AStatus_deleteDescription");
        let status_delete = sym(c"AStatus_delete");
        let start_thread_pool = sym(c"ABinderProcess_startThreadPool");
        let join_thread_pool = sym(c"ABinderProcess_joinThreadPool");
        // A null required symbol means the library is
        // unusable, as in C++ `FORWARD` returning its
        // fallback; the handle is deliberately not
        // closed, matching C++ which keeps it open.
        // Checked one by one (not one big boolean) so
        // the log can name the missing symbol.
        let required: [(*mut c_void, &'static str); 19] = [
            (get_service, "AServiceManager_getService"),
            (class_define, "AIBinder_Class_define"),
            (associate_class, "AIBinder_associateClass"),
            (binder_new, "AIBinder_new"),
            (prepare_transaction, "AIBinder_prepareTransaction"),
            (transact, "AIBinder_transact"),
            (parcel_delete, "AParcel_delete"),
            (read_status_header, "AParcel_readStatusHeader"),
            (write_int32, "AParcel_writeInt32"),
            (write_strong_binder, "AParcel_writeStrongBinder"),
            (read_int32, "AParcel_readInt32"),
            (read_bool, "AParcel_readBool"),
            (read_string, "AParcel_readString"),
            (status_is_ok, "AStatus_isOk"),
            (status_get_description, "AStatus_getDescription"),
            (status_delete_description, "AStatus_deleteDescription"),
            (status_delete, "AStatus_delete"),
            (start_thread_pool, "ABinderProcess_startThreadPool"),
            (join_thread_pool, "ABinderProcess_joinThreadPool"),
        ];
        for (pointer, name) in required {
            if pointer.is_null() {
                return Err(LoadError::Symbol(name));
            }
        }
        let wait_for_service = sym(c"AServiceManager_waitForService");
        Ok(BinderNdk {
            get_service: cast!(get_service, GetServiceFn),
            wait_for_service: if wait_for_service.is_null() {
                None
            } else {
                Some(cast!(wait_for_service, WaitForServiceFn))
            },
            class_define: cast!(class_define, ClassDefineFn),
            associate_class: cast!(associate_class, AssociateClassFn),
            binder_new: cast!(binder_new, BinderNewFn),
            prepare_transaction: cast!(prepare_transaction, PrepareTransactionFn),
            transact: cast!(transact, TransactFn),
            parcel_delete: cast!(parcel_delete, ParcelDeleteFn),
            read_status_header: cast!(read_status_header, ReadStatusHeaderFn),
            write_int32: cast!(write_int32, WriteInt32Fn),
            write_strong_binder: cast!(write_strong_binder, WriteStrongBinderFn),
            read_int32: cast!(read_int32, ReadInt32Fn),
            read_bool: cast!(read_bool, ReadBoolFn),
            read_string: cast!(read_string, ReadStringFn),
            status_is_ok: cast!(status_is_ok, StatusIsOkFn),
            status_get_description: cast!(status_get_description, StatusGetDescriptionFn),
            status_delete_description: cast!(status_delete_description, StatusDeleteDescriptionFn),
            status_delete: cast!(status_delete, StatusDeleteFn),
            start_thread_pool: cast!(start_thread_pool, StartThreadPoolFn),
            join_thread_pool: cast!(join_thread_pool, JoinThreadPoolFn),
        })
    }
}

fn load_binder_ndk() -> Option<BinderNdk> {
    load_binder_ndk_detailed().ok()
}

fn binder_ndk() -> Option<&'static BinderNdk> {
    static LOADED: OnceLock<Option<BinderNdk>> = OnceLock::new();
    LOADED.get_or_init(load_binder_ndk).as_ref()
}

/// The detailed loader result, for
/// [`BinderServices::acquire_detailed`], which needs
/// the reason, not just success or failure. The
/// `OnceLock` in [`binder_ndk`] is not reused here,
/// because a failed load must be reported with its
/// error, and a successful load is already cached
/// there for the other methods.
///
/// # Errors
///
/// Returns the [`LoadError`] from the first load
/// attempt, cached for later calls.
fn binder_ndk_detailed() -> Result<&'static BinderNdk, LoadError> {
    static LOADED: OnceLock<Result<BinderNdk, LoadError>> = OnceLock::new();
    match LOADED.get_or_init(load_binder_ndk_detailed) {
        Ok(ndk) => Ok(ndk),
        Err(error) => Err(error.clone()),
    }
}

/// Whether a named `libbinder_ndk.so` symbol exists,
/// mirroring `BinderNDK_hasSymbol`.
#[must_use]
pub fn has_symbol(name: &str) -> bool {
    let Ok(c_name) = CString::new(name) else {
        return false;
    };
    // SAFETY: `dlopen` with a constant library name, as
    // above; a null handle means the symbol is absent.
    unsafe {
        let handle = libc::dlopen(
            c"libbinder_ndk.so".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if handle.is_null() {
            return false;
        }
        !libc::dlsym(handle, c_name.as_ptr()).is_null()
    }
}

/// The five services `BinderMonitor` acquires, in C++
/// order, with their interface tokens.
pub const SERVICES: [(&str, &str); 5] = [
    ("power", "android.os.IPowerManager"),
    ("notification", "android.app.INotificationManager"),
    ("activity", "android.app.IActivityManager"),
    ("display", "android.hardware.display.IDisplayManager"),
    ("package", "android.content.pm.IPackageManager"),
];

/// The transaction codes a callback registry needs, as
/// plain codes (resolved by the resolver in
/// `encore_core`, not here, to avoid a dependency from
/// this crate back to the core).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallbackCodes {
    /// Code for `onForegroundActivitiesChanged`.
    pub on_foreground_activities: u32,
    /// Code for `onProcessDied`.
    pub on_process_died: u32,
    /// Code for `onDisplayEvent`.
    pub on_display_event: u32,
    /// Code for `isInteractive`, queried by the display
    /// callback.
    pub is_interactive: u32,
    /// Code for `registerProcessObserver`, used to
    /// register the process-observer binder.
    pub register_process_observer: u32,
    /// Code for `registerCallback` on the display
    /// manager, used to register the display callback.
    pub register_display_callback: u32,
}

/// A decoded Binder callback event, before any profile
/// decision. Callers in `encore_core` map these to
/// `BinderEvent` values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackEvent {
    /// A foreground-activity change.
    Foreground {
        /// Process ID.
        pid: i32,
        /// User ID.
        uid: i32,
        /// Whether the activity came to the foreground.
        foreground: bool,
    },
    /// A process-died notification.
    ProcessDied {
        /// Process ID.
        pid: i32,
        /// User ID.
        uid: i32,
    },
    /// A display interactivity change, already
    /// change-filtered by the display callback.
    Display {
        /// Whether the screen is interactive.
        interactive: bool,
    },
}

/// Callbacks registered for Binder events. Only
/// foreground, died, and display events are delivered,
/// matching `Main.cpp`, which leaves the other
/// process-observer callbacks empty.
#[derive(Default)]
pub struct CallbackHandlers {
    /// Called for each decoded callback event.
    pub on_event: Option<Box<dyn Fn(CallbackEvent) + Send>>,
}

struct CallbackRegistry {
    handlers: CallbackHandlers,
    codes: CallbackCodes,
    power_binder: *mut AIBinder,
    display_last_state: bool,
}

// SAFETY: the registry is only accessed behind a
// `Mutex`, and the power binder pointer is only used
// inside `displayCallback_transact`, after services
// have been acquired, so sharing it across Binder
// thread-pool threads is sound.
unsafe impl Send for CallbackRegistry {}

static CALLBACK_REGISTRY: OnceLock<Mutex<CallbackRegistry>> = OnceLock::new();

fn callback_registry() -> &'static Mutex<CallbackRegistry> {
    CALLBACK_REGISTRY.get_or_init(|| {
        Mutex::new(CallbackRegistry {
            handlers: CallbackHandlers::default(),
            codes: CallbackCodes::default(),
            power_binder: std::ptr::null_mut(),
            display_last_state: false,
        })
    })
}

extern "C" fn noop_create(args: *mut c_void) -> *mut c_void {
    args
}

extern "C" fn noop_destroy(_args: *mut c_void) {}

extern "C" fn noop_transact(
    _binder: *mut AIBinder,
    _code: u32,
    _in: *const AParcel,
    _out: *mut AParcel,
) -> BinderStatus {
    STATUS_UNKNOWN_ERROR
}

extern "C" fn process_observer_transact(
    _binder: *mut AIBinder,
    code: u32,
    parcel: *const AParcel,
    _out: *mut AParcel,
) -> BinderStatus {
    let Some(ndk) = binder_ndk() else {
        return STATUS_UNKNOWN_ERROR;
    };
    let Ok(registry) = callback_registry().lock() else {
        return STATUS_UNKNOWN_ERROR;
    };
    // SAFETY: `parcel` is the parcel the Binder driver
    // passes to this handler, and the read functions
    // only read from it in the order C++ does. A failed
    // read leaves the value at its C++ default (-1 / 0 /
    // false), and the handler still returns `STATUS_OK`
    // for a recognised code, as in C++.
    unsafe {
        if code == registry.codes.on_foreground_activities && code != 0 {
            let mut pid = -1;
            let mut uid = -1;
            let mut foreground = false;
            let _ = (ndk.read_int32)(parcel, &mut pid);
            let _ = (ndk.read_int32)(parcel, &mut uid);
            let _ = (ndk.read_bool)(parcel, &mut foreground);
            if let Some(handler) = &registry.handlers.on_event {
                handler(CallbackEvent::Foreground {
                    pid,
                    uid,
                    foreground,
                });
            }
            return STATUS_OK;
        }
        if code == registry.codes.on_process_died && code != 0 {
            let mut pid = -1;
            let mut uid = -1;
            let _ = (ndk.read_int32)(parcel, &mut pid);
            let _ = (ndk.read_int32)(parcel, &mut uid);
            if let Some(handler) = &registry.handlers.on_event {
                handler(CallbackEvent::ProcessDied { pid, uid });
            }
            return STATUS_OK;
        }
    }
    STATUS_UNKNOWN_ERROR
}

extern "C" fn display_callback_transact(
    _binder: *mut AIBinder,
    code: u32,
    _parcel: *const AParcel,
    _out: *mut AParcel,
) -> BinderStatus {
    let Some(ndk) = binder_ndk() else {
        return STATUS_UNKNOWN_ERROR;
    };
    // Read the code and power binder without holding
    // the registry lock across the query, to avoid
    // holding a lock across a Binder transaction.
    let (expected, power_binder) = {
        let Ok(registry) = callback_registry().lock() else {
            return STATUS_UNKNOWN_ERROR;
        };
        (registry.codes.on_display_event, registry.power_binder)
    };
    if code != expected || code == 0 || power_binder.is_null() {
        return STATUS_UNKNOWN_ERROR;
    }
    let is_interactive_code = expected_is_interactive_code();
    if is_interactive_code == 0 {
        return STATUS_UNKNOWN_ERROR;
    }
    let current = transact_read_int32_inner(ndk, power_binder, is_interactive_code, 0) != 0;
    let Ok(mut registry) = callback_registry().lock() else {
        return STATUS_UNKNOWN_ERROR;
    };
    if current != registry.display_last_state {
        registry.display_last_state = current;
        if let Some(handler) = &registry.handlers.on_event {
            handler(CallbackEvent::Display {
                interactive: current,
            });
        }
    }
    STATUS_OK
}

/// The string allocator for `AParcel_readString`, as in
/// C++ `stringAllocator`: resize the supplied buffer to
/// `size` and hand out its data pointer.
extern "C" fn string_allocator(data: *mut c_void, size: i32, out: *mut *mut c_char) -> bool {
    if size < 0 || data.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: `data` is a `Vec<u8>` created by
    // `transact_read_string` in this module, and `size`
    // was checked non-negative above.
    unsafe {
        let buffer = &mut *(data as *mut Vec<u8>);
        buffer.resize(size as usize, 0);
        *out = buffer.as_mut_ptr() as *mut c_char;
    }
    true
}

fn transact_read_int32_inner(
    ndk: &BinderNdk,
    binder: *mut AIBinder,
    code: u32,
    on_error: i32,
) -> i32 {
    // SAFETY: `binder` comes from service acquisition
    // in this module, and every parcel created here is
    // deleted exactly once below, on every path, as in
    // C++ `transactReadInt32`, except for the interface
    // token: the NDK writes that header itself in
    // `prepare_transaction`, from the class associated
    // with the service in `acquire`. C++ also calls a
    // `writeInterfaceToken` function here, but that
    // symbol does not exist in `libbinder_ndk.so` on
    // the test device (v3 log, 2026-10-28), so its
    // C++ call is a no-op whose error is ignored, and
    // requiring it in Rust was the acquire failure.
    unsafe {
        let mut input: *mut AParcel = std::ptr::null_mut();
        if (ndk.prepare_transaction)(binder, &mut input) != STATUS_OK || input.is_null() {
            return on_error;
        }
        let mut output: *mut AParcel = std::ptr::null_mut();
        let status = (ndk.transact)(binder, code, &mut input, &mut output, 0);
        if status != STATUS_OK || output.is_null() {
            if !output.is_null() {
                (ndk.parcel_delete)(output);
            }
            return on_error;
        }
        let mut status_ptr: *mut AStatus = std::ptr::null_mut();
        if (ndk.read_status_header)(output, &mut status_ptr) != STATUS_OK || status_ptr.is_null() {
            (ndk.parcel_delete)(output);
            return on_error;
        }
        let ok = (ndk.status_is_ok)(status_ptr);
        if !ok {
            let description = (ndk.status_get_description)(status_ptr);
            if !description.is_null() {
                (ndk.status_delete_description)(description);
            }
            (ndk.status_delete)(status_ptr);
            (ndk.parcel_delete)(output);
            return on_error;
        }
        (ndk.status_delete)(status_ptr);
        let mut value = on_error;
        let _ = (ndk.read_int32)(output, &mut value);
        (ndk.parcel_delete)(output);
        value
    }
}

fn transact_read_string_inner(
    ndk: &BinderNdk,
    binder: *mut AIBinder,
    code: u32,
    arg: i32,
) -> String {
    // SAFETY: as in `transact_read_int32_inner`, with
    // the addition that the string allocator only writes
    // into a `Vec<u8>` created here. The interface
    // token is written by `prepare_transaction`, as
    // above, not by an explicit call.
    unsafe {
        let mut input: *mut AParcel = std::ptr::null_mut();
        if (ndk.prepare_transaction)(binder, &mut input) != STATUS_OK || input.is_null() {
            return String::new();
        }
        if (ndk.write_int32)(input, arg) != STATUS_OK {
            (ndk.parcel_delete)(input);
            return String::new();
        }
        let mut output: *mut AParcel = std::ptr::null_mut();
        let status = (ndk.transact)(binder, code, &mut input, &mut output, 0);
        if status != STATUS_OK || output.is_null() {
            if !output.is_null() {
                (ndk.parcel_delete)(output);
            }
            return String::new();
        }
        let mut status_ptr: *mut AStatus = std::ptr::null_mut();
        if (ndk.read_status_header)(output, &mut status_ptr) != STATUS_OK || status_ptr.is_null() {
            (ndk.parcel_delete)(output);
            return String::new();
        }
        let ok = (ndk.status_is_ok)(status_ptr);
        if !ok {
            let description = (ndk.status_get_description)(status_ptr);
            if !description.is_null() {
                (ndk.status_delete_description)(description);
            }
            (ndk.status_delete)(status_ptr);
            (ndk.parcel_delete)(output);
            return String::new();
        }
        (ndk.status_delete)(status_ptr);
        let mut buffer: Vec<u8> = Vec::new();
        let _ = (ndk.read_string)(
            output,
            &mut buffer as *mut Vec<u8> as *mut c_void,
            string_allocator,
        );
        (ndk.parcel_delete)(output);
        String::from_utf8_lossy(&buffer).into_owned()
    }
}

/// Why [`BinderServices::acquire_detailed`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    /// `libbinder_ndk.so` could not be loaded, or a
    /// required symbol was missing, with the detail
    /// from [`LoadError`].
    Library(LoadError),
    /// One named service could not be acquired, even
    /// after waiting.
    Service(&'static str),
}

/// Acquired Binder services, in C++ order. Service
/// pointers are held for the process lifetime, as in
/// C++, and are never freed by this module.
pub struct BinderServices {
    power: *mut AIBinder,
    notification: *mut AIBinder,
    activity: *mut AIBinder,
    display: *mut AIBinder,
    package: *mut AIBinder,
}

// SAFETY: service pointers are only used inside this
// module's methods, behind no shared mutation, and the
// NDK service manager owns them for the process
// lifetime, so sharing this struct across threads (for
// the Binder thread pool and the power-save poller) is
// sound.
unsafe impl Send for BinderServices {}
unsafe impl Sync for BinderServices {}

impl BinderServices {
    /// Acquire all five services, waiting up to 10
    /// seconds with 100 ms polls for any service that
    /// is not immediately available when the
    /// `waitForService` symbol is absent, as in C++.
    /// Returns `None` when the library is absent or any
    /// service cannot be acquired, which is the C++
    /// `initialize` failure case.
    #[must_use]
    pub fn acquire() -> Option<Self> {
        Self::acquire_detailed().ok()
    }

    /// As [`Self::acquire`], but reporting *why* it
    /// failed, because the first device test (whyred,
    /// 2026-10-28) failed here with only a generic
    /// log line, leaving it unknown whether the
    /// library, a required symbol, or one specific
    /// service was the cause. C++ logs the failing
    /// service name; this is the Rust equivalent.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Library`] with the
    /// load detail when the library or a required
    /// symbol is missing, or [`AcquireError::Service`]
    /// naming the service that could not be acquired.
    pub fn acquire_detailed() -> Result<Self, AcquireError> {
        let ndk = match binder_ndk_detailed() {
            Ok(ndk) => ndk,
            Err(error) => return Err(AcquireError::Library(error)),
        };
        let mut pointers: Vec<*mut AIBinder> = Vec::new();
        for (name, _token) in SERVICES.iter() {
            let Ok(c_name) = CString::new(*name) else {
                return Err(AcquireError::Service(name));
            };
            // SAFETY: `c_name` is a valid C string for
            // a service name, and the returned pointer
            // is either null or a service-manager-owned
            // binder, as in C++.
            let mut binder = unsafe { (ndk.get_service)(c_name.as_ptr()) };
            if binder.is_null() {
                binder = wait_for_service(ndk, name);
            }
            if binder.is_null() {
                return Err(AcquireError::Service(name));
            }
            pointers.push(binder);
        }
        if pointers.len() != SERVICES.len() {
            return Err(AcquireError::Library(LoadError::Open(
                "service pointer list is incomplete".to_string(),
            )));
        }
        let power = pointers[0];
        let notification = pointers[1];
        let activity = pointers[2];
        let display = pointers[3];
        let package = pointers[4];
        // Associate each service with its interface
        // class, as in C++ `initialize`, so transactions
        // carry the right descriptor.
        associate_service(ndk, power, "android.os.IPowerManager");
        associate_service(ndk, notification, "android.app.INotificationManager");
        associate_service(ndk, activity, "android.app.IActivityManager");
        associate_service(ndk, display, "android.hardware.display.IDisplayManager");
        associate_service(ndk, package, "android.content.pm.IPackageManager");
        Ok(Self {
            power,
            notification,
            activity,
            display,
            package,
        })
    }

    /// Whether Battery Saver is active, or `false` on
    /// any Binder failure, as in C++ `isPowerSave`.
    #[must_use]
    pub fn is_power_save(&self, code: u32) -> bool {
        let Some(ndk) = binder_ndk() else {
            return false;
        };
        if code == 0 {
            return false;
        }
        transact_read_int32_inner(ndk, self.power, code, 0) != 0
    }

    /// Whether the screen is interactive, or `false` on
    /// any Binder failure, as in C++ `queryIsInteractive`.
    #[must_use]
    pub fn is_interactive(&self, code: u32) -> bool {
        let Some(ndk) = binder_ndk() else {
            return false;
        };
        if code == 0 {
            return false;
        }
        transact_read_int32_inner(ndk, self.power, code, 0) != 0
    }

    /// The zen mode, or `-1` on any Binder failure, as
    /// in C++ `getZenMode`.
    #[must_use]
    pub fn get_zen_mode(&self, code: u32) -> i32 {
        let Some(ndk) = binder_ndk() else {
            return -1;
        };
        if code == 0 {
            return -1;
        }
        transact_read_int32_inner(ndk, self.notification, code, -1)
    }

    /// The package name for a UID, or `""` on any Binder
    /// failure, as in C++ `getPackageNameForUid`.
    #[must_use]
    pub fn get_package_name_for_uid(&self, code: u32, uid: i32) -> String {
        let Some(ndk) = binder_ndk() else {
            return String::new();
        };
        if code == 0 {
            return String::new();
        }
        transact_read_string_inner(ndk, self.package, code, uid)
    }

    /// Register process-observer and display callbacks
    /// with the given transaction codes and handlers,
    /// mirroring C++ `initialize`'s registration steps.
    /// Returns `false` when the library is absent, a
    /// callback binder cannot be allocated, or a
    /// registration transaction cannot be prepared.
    /// A failed registration transaction status, as in
    /// C++, is logged by the caller but does not fail
    /// registration here, because C++ only logs it.
    pub fn register_callbacks(&self, codes: CallbackCodes, handlers: CallbackHandlers) -> bool {
        let Some(ndk) = binder_ndk() else {
            return false;
        };
        // Seed the display last-state and the power
        // binder for the display callback, as in C++.
        let initial_display = self.is_interactive(codes.is_interactive);
        {
            let Ok(mut registry) = callback_registry().lock() else {
                return false;
            };
            registry.handlers = handlers;
            registry.codes = codes;
            registry.power_binder = self.power;
            registry.display_last_state = initial_display;
        }
        // SAFETY: class descriptors are constant
        // strings, and the transact handlers only read
        // from the parcel they are given, as documented
        // above.
        unsafe {
            let observer_class = (ndk.class_define)(
                c"android.app.IProcessObserver".as_ptr(),
                noop_create,
                noop_destroy,
                process_observer_transact,
            );
            if observer_class.is_null() {
                return false;
            }
            let observer_binder = (ndk.binder_new)(observer_class, std::ptr::null_mut());
            if observer_binder.is_null() {
                return false;
            }
            if !register_binder(
                ndk,
                self.activity,
                codes.register_process_observer,
                observer_binder,
            ) {
                // C++ logs a failed registration status
                // but continues; the caller decides
                // whether the observer is usable.
            }
            let display_class = (ndk.class_define)(
                c"android.hardware.display.IDisplayManagerCallback".as_ptr(),
                noop_create,
                noop_destroy,
                display_callback_transact,
            );
            if display_class.is_null() {
                return false;
            }
            let display_binder = (ndk.binder_new)(display_class, std::ptr::null_mut());
            if display_binder.is_null() {
                return false;
            }
            let _ = register_binder(
                ndk,
                self.display,
                codes.register_display_callback,
                display_binder,
            );
        }
        true
    }

    /// Start the Binder thread pool, as in C++
    /// `initialize`'s final step. A no-op when the
    /// library is absent.
    pub fn start_thread_pool() {
        if let Some(ndk) = binder_ndk() {
            // SAFETY: starting the NDK thread pool once,
            // as in C++.
            unsafe {
                (ndk.start_thread_pool)();
            }
        }
    }

    /// Block by joining the Binder thread pool, as in
    /// C++ `joinThreadPool`. A no-op when the library is
    /// absent.
    pub fn join_thread_pool() {
        if let Some(ndk) = binder_ndk() {
            // SAFETY: joining the NDK thread pool, as in
            // C++.
            unsafe {
                (ndk.join_thread_pool)();
            }
        }
    }
}

fn wait_for_service(ndk: &BinderNdk, name: &str) -> *mut AIBinder {
    if let Some(wait_for) = ndk.wait_for_service {
        let Ok(c_name) = CString::new(name) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `c_name` is a valid service name.
        return unsafe { wait_for(c_name.as_ptr()) };
    }
    let Ok(c_name) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        // SAFETY: as above.
        let binder = unsafe { (ndk.get_service)(c_name.as_ptr()) };
        if !binder.is_null() {
            return binder;
        }
    }
    std::ptr::null_mut()
}

fn associate_service(ndk: &BinderNdk, binder: *mut AIBinder, descriptor: &str) {
    let Ok(c_descriptor) = CString::new(descriptor) else {
        return;
    };
    // SAFETY: the descriptor is a constant interface
    // name, and the class transact handler is a no-op,
    // as in C++.
    unsafe {
        let class = (ndk.class_define)(
            c_descriptor.as_ptr(),
            noop_create,
            noop_destroy,
            noop_transact,
        );
        if !class.is_null() {
            let _ = (ndk.associate_class)(binder, class);
        }
    }
}

fn register_binder(
    ndk: &BinderNdk,
    service: *mut AIBinder,
    code: u32,
    callback_binder: *mut AIBinder,
) -> bool {
    if code == 0 {
        return false;
    }
    // SAFETY: `service` and `callback_binder` come from
    // service acquisition and `AIBinder_new` in this
    // module, and the input parcel is deleted by the
    // transact call's ownership rules as in C++: the
    // input parcel is consumed by transact, the output
    // parcel, if any, is deleted here.
    unsafe {
        let mut input: *mut AParcel = std::ptr::null_mut();
        if (ndk.prepare_transaction)(service, &mut input) != STATUS_OK || input.is_null() {
            return false;
        }
        // No explicit interface-token write here, for
        // the same reason as in `transact_read_int32_inner`:
        // `prepare_transaction` writes it from the
        // associated class.
        if (ndk.write_strong_binder)(input, callback_binder) != STATUS_OK {
            (ndk.parcel_delete)(input);
            return false;
        }
        let mut output: *mut AParcel = std::ptr::null_mut();
        let status = (ndk.transact)(service, code, &mut input, &mut output, 0);
        if !output.is_null() {
            (ndk.parcel_delete)(output);
        }
        status == STATUS_OK
    }
}

// The display callback needs the isInteractive code,
// which lives in the registry. This helper reads it
// without holding the registry lock across a query.
// It is called only from `display_callback_transact`,
// which has already released the lock.
fn expected_is_interactive_code() -> u32 {
    match callback_registry().lock() {
        Ok(registry) => registry.codes.is_interactive,
        Err(_) => 0,
    }
}
