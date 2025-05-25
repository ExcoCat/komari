use std::{cell::Cell, ffi::OsString, os::windows::ffi::OsStringExt, ptr, str};

use windows::Win32::{
    Foundation::{BOOL, HWND, LPARAM},
    Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
    UI::WindowsAndMessaging::{
        EnumWindows, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetWindowLongPtrW, GetWindowTextW,
        IsWindowVisible, WS_DISABLED, WS_EX_TOOLWINDOW, WS_POPUP,
    },
};

#[derive(Clone, Debug)]
pub(crate) struct HandleCell {
    handle: Handle,
    inner: Cell<Option<HWND>>,
}

impl HandleCell {
    pub fn new(handle: Handle) -> Self {
        Self {
            handle,
            inner: Cell::new(None),
        }
    }

    #[inline]
    pub fn as_inner(&self) -> Option<HWND> {
        match self.handle.kind {
            HandleKind::Fixed(_) => self.handle.query_handle(),
            HandleKind::Dynamic(class, is_popup) => {
                if self.inner.get().is_none() {
                    self.inner.set(self.handle.query_handle());
                }
                let handle_inner = self.inner.get()?;
                if is_class_matched(handle_inner, class, is_popup) {
                    return Some(handle_inner);
                }
                self.inner.set(None);
                None
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HandleKind {
    Fixed(HWND),
    Dynamic(&'static str, bool),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Handle {
    kind: HandleKind,
}

impl Handle {
    /// Creates a new `Handle` with `class`.
    ///
    /// If `is_popup` is true, multiple `Handle`s matching `class` will prioritize one with
    /// `WS_POPUP` style.
    ///
    /// TODO: is_popup is adhoc for finding MapleStory external chat box, should have a better
    /// way to filter in case there is a need for that
    pub fn new(class: &'static str, is_popup: bool) -> Self {
        Self {
            kind: HandleKind::Dynamic(class, is_popup),
        }
    }

    pub(crate) fn new_fixed(handle: HWND) -> Self {
        Self {
            kind: HandleKind::Fixed(handle),
        }
    }

    fn query_handle(&self) -> Option<HWND> {
        match self.kind {
            HandleKind::Fixed(handle) => Some(handle),
            HandleKind::Dynamic(class, is_popup) => {
                struct Params {
                    class: &'static str,
                    is_popup: bool,
                    handle_out: *mut HWND,
                }

                unsafe extern "system" fn callback(handle: HWND, params: LPARAM) -> BOOL {
                    let params = unsafe { ptr::read::<Params>(params.0 as *const _) };
                    if is_class_matched(handle, params.class, params.is_popup) {
                        unsafe { ptr::write(params.handle_out, handle) };
                        false.into()
                    } else {
                        true.into()
                    }
                }

                let mut handle = HWND::default();
                let params = Params {
                    class,
                    is_popup,
                    handle_out: &raw mut handle,
                };
                let _ = unsafe { EnumWindows(Some(callback), LPARAM(&raw const params as isize)) };
                (!handle.is_invalid()).then_some(handle)
            }
        }
    }
}

pub fn query_capture_handles() -> Vec<(String, Handle)> {
    unsafe extern "system" fn callback(handle: HWND, params: LPARAM) -> BOOL {
        if !unsafe { IsWindowVisible(handle) }.as_bool() {
            return true.into();
        }

        let mut cloaked = 0u32;
        let _ = unsafe {
            DwmGetWindowAttribute(
                handle,
                DWMWA_CLOAKED,
                (&raw mut cloaked).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        };
        if cloaked != 0 {
            return true.into();
        }

        let style = unsafe { GetWindowLongPtrW(handle, GWL_STYLE) } as u32;
        let ex_style = unsafe { GetWindowLongPtrW(handle, GWL_EXSTYLE) } as u32;
        if style & WS_DISABLED.0 != 0 || ex_style & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }

        let mut buf = [0u16; 256];
        let count = unsafe { GetWindowTextW(handle, &mut buf) } as usize;
        if count == 0 {
            return true.into();
        }

        let vec = unsafe { &mut *(params.0 as *mut Vec<(String, Handle)>) };
        if let Some(name) = OsString::from_wide(&buf[..count]).to_str() {
            vec.push((name.to_string(), Handle::new_fixed(handle)));
        }
        true.into()
    }

    let mut vec = Vec::new();
    let _ = unsafe { EnumWindows(Some(callback), LPARAM(&raw mut vec as isize)) };
    vec
}

#[inline]
fn is_class_matched(handle: HWND, class: &'static str, is_popup: bool) -> bool {
    let mut buf = [0u16; 256];
    let count = unsafe { GetClassNameW(handle, &mut buf) as usize };
    if count == 0 {
        return false;
    }

    let class_match = OsString::from_wide(&buf[..count])
        .to_str()
        .map(|s| s.starts_with(class))
        .unwrap_or(false);
    if !class_match {
        return false;
    }

    let style = unsafe { GetWindowLongPtrW(handle, GWL_STYLE) } as u32;
    let has_style = (style & WS_POPUP.0) != 0;
    is_popup == has_style
}
