//! Barra di progresso sull'icona della taskbar (ITaskbarList3).
//!
//! `windows-sys` espone funzioni e costanti ma non le interfacce COM, quindi
//! la vtable è dichiarata qui a mano seguendo l'ordine di ereditarietà
//! IUnknown -> ITaskbarList -> ITaskbarList2 -> ITaskbarList3. Ogni passo
//! (CoCreateInstance, HrInit) è controllato: se qualcosa non torna si resta
//! semplicemente senza barra, mai un crash.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::core::{GUID, HRESULT};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };

    // CLSID_TaskbarList {56FDF344-FD6D-11d0-958A-006097C9A090}
    const CLSID_TASKBAR_LIST: GUID =
        GUID::from_u128(0x56FDF344_FD6D_11d0_958A_006097C9A090);
    // IID_ITaskbarList3 {EA1AFB91-9E28-4B86-90E9-9E9F8A5EEFAF}
    const IID_ITASKBAR_LIST3: GUID =
        GUID::from_u128(0xEA1AFB91_9E28_4B86_90E9_9E9F8A5EEFAF);

    pub const TBPF_NOPROGRESS: u32 = 0x0;
    pub const TBPF_NORMAL: u32 = 0x2;
    pub const TBPF_ERROR: u32 = 0x4;
    pub const TBPF_PAUSED: u32 = 0x8;

    /// Vtable di ITaskbarList3. L'ordine è quello della catena di ereditarietà
    /// e NON va toccato: ogni voce è uno slot preciso.
    #[repr(C)]
    struct ITaskbarList3Vtbl {
        // IUnknown
        query_interface: unsafe extern "system" fn(*mut Obj, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut Obj) -> u32,
        release: unsafe extern "system" fn(*mut Obj) -> u32,
        // ITaskbarList
        hr_init: unsafe extern "system" fn(*mut Obj) -> HRESULT,
        add_tab: unsafe extern "system" fn(*mut Obj, HWND) -> HRESULT,
        delete_tab: unsafe extern "system" fn(*mut Obj, HWND) -> HRESULT,
        activate_tab: unsafe extern "system" fn(*mut Obj, HWND) -> HRESULT,
        set_active_alt: unsafe extern "system" fn(*mut Obj, HWND) -> HRESULT,
        // ITaskbarList2
        mark_fullscreen_window: unsafe extern "system" fn(*mut Obj, HWND, i32) -> HRESULT,
        // ITaskbarList3 (solo i due che ci servono; il resto non viene mai chiamato)
        set_progress_value: unsafe extern "system" fn(*mut Obj, HWND, u64, u64) -> HRESULT,
        set_progress_state: unsafe extern "system" fn(*mut Obj, HWND, u32) -> HRESULT,
    }

    #[repr(C)]
    struct Obj {
        vtbl: *const ITaskbarList3Vtbl,
    }

    thread_local! {
        /// Il puntatore COM è legato al thread che l'ha creato (apartment):
        /// vive solo sul thread della UI, che è anche l'unico che lo usa.
        static TASKBAR: std::cell::Cell<*mut Obj> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    }

    static TRIED: AtomicBool = AtomicBool::new(false);

    /// Crea l'oggetto una volta sola. Null = niente barra, si tira avanti.
    fn get() -> *mut Obj {
        let existing = TASKBAR.with(|t| t.get());
        if !existing.is_null() || TRIED.swap(true, Ordering::SeqCst) {
            return existing;
        }
        unsafe {
            // winit ha gia' inizializzato COM su questo thread; se cosi' non
            // fosse ci pensa questa chiamata, e un eventuale RPC_E_CHANGED_MODE
            // e' innocuo perche' verifichiamo comunque il risultato dopo.
            let _ = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            let hr = CoCreateInstance(
                &CLSID_TASKBAR_LIST,
                std::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ITASKBAR_LIST3,
                &mut ptr,
            );
            if hr < 0 || ptr.is_null() {
                return std::ptr::null_mut();
            }
            let obj = ptr as *mut Obj;
            // HrInit deve riuscire prima di qualsiasi altra chiamata
            if ((*(*obj).vtbl).hr_init)(obj) < 0 {
                ((*(*obj).vtbl).release)(obj);
                return std::ptr::null_mut();
            }
            TASKBAR.with(|t| t.set(obj));
            obj
        }
    }

    /// `done`/`total` a 0 con stato NOPROGRESS spegne la barra.
    /// Ritorna false se la taskbar non è disponibile (COM non inizializzato,
    /// shell sostituita, finestra non ancora creata): il chiamante lo logga.
    pub fn set(hwnd: isize, state: u32, done: u64, total: u64) -> bool {
        if hwnd == 0 {
            return false;
        }
        let obj = get();
        if obj.is_null() {
            return false;
        }
        unsafe {
            ((*(*obj).vtbl).set_progress_state)(obj, hwnd as HWND, state);
            if state != TBPF_NOPROGRESS && total > 0 {
                ((*(*obj).vtbl).set_progress_value)(obj, hwnd as HWND, done, total);
            }
        }
        true
    }
}

#[cfg(windows)]
pub use imp::{set, TBPF_ERROR, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED};

#[cfg(not(windows))]
pub const TBPF_NOPROGRESS: u32 = 0;
#[cfg(not(windows))]
pub const TBPF_NORMAL: u32 = 2;
#[cfg(not(windows))]
pub const TBPF_ERROR: u32 = 4;
#[cfg(not(windows))]
pub const TBPF_PAUSED: u32 = 8;

#[cfg(not(windows))]
pub fn set(_hwnd: isize, _state: u32, _done: u64, _total: u64) -> bool {
    false
}
