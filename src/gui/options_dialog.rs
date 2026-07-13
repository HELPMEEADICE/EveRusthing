use std::mem::size_of;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::Once;

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, HTREEITEM, NMTREEVIEWW, TVE_EXPAND, TVGN_CARET, TVI_LAST, TVI_ROOT, TVIF_PARAM,
    TVIF_TEXT, TVINSERTSTRUCTW, TVM_EXPAND, TVM_INSERTITEMW, TVM_SELECTITEM, TVN_SELCHANGEDW,
    TVS_HASBUTTONS, TVS_HASLINES, TVS_LINESATROOT, TVS_SHOWSELALWAYS, WC_TREEVIEWW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_PUSHBUTTON,
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClientRect, GetDlgItem, GetMessageW, GetWindowLongPtrW, GetWindowRect,
    IDC_ARROW, IsDialogMessageW, IsWindow, LoadCursorW, MSG, MoveWindow, PostQuitMessage,
    RegisterClassExW, SW_HIDE, SW_SHOW, SendMessageW, SetWindowLongPtrW, SetWindowTextW,
    ShowWindow, TranslateMessage, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_NCCREATE,
    WM_NOTIFY, WM_SETFONT, WM_SIZE, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN,
    WS_EX_DLGMODALFRAME, WS_GROUP, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use crate::query::QueryOptions;

const CLASS_NAME: &str = "EVERUSTHING_OPTIONS";
const TITLE: &str = "Everything Options";
const ID_TREE: usize = 200;
const ID_OK: usize = 201;
const ID_CANCEL: usize = 202;
const ID_APPLY: usize = 203;
const ID_STATUS_BAR: usize = 210;
const ID_SELECTED_STATUS: usize = 211;
const ID_SEARCH_AS_TYPE: usize = 212;
const ID_MATCH_CASE: usize = 213;
const ID_MATCH_WHOLE_WORD: usize = 214;
const ID_MATCH_PATH: usize = 215;
const ID_FORCE_REBUILD: usize = 216;

const PAGE_GENERAL: usize = 0;
const PAGE_VIEW: usize = 1;
const PAGE_SEARCH: usize = 2;
const PAGE_INDEXES: usize = 3;
const PAGE_NTFS: usize = 4;
const PAGE_TITLES: [&str; 5] = ["General", "View", "Search", "Indexes", "NTFS"];

static REGISTER_CLASS: Once = Once::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OptionsConfig {
    pub show_status_bar: bool,
    pub show_selected_item_in_statusbar: bool,
    pub search_as_you_type: bool,
    pub query: QueryOptions,
}

pub(super) struct AppliedOptions {
    pub config: OptionsConfig,
    pub force_rebuild: bool,
}

struct PageControl {
    window: HWND,
    page: usize,
    height: i32,
}

struct OptionsState {
    parent: HWND,
    apply_message: u32,
    tree: HWND,
    title: HWND,
    page_controls: Vec<PageControl>,
    status_bar: HWND,
    selected_status: HWND,
    search_as_type: HWND,
    match_case: HWND,
    match_whole_word: HWND,
    match_path: HWND,
    force_rebuild: HWND,
    apply: HWND,
    force_rebuild_pending: bool,
}

struct CreateState {
    parent: HWND,
    apply_message: u32,
    config: OptionsConfig,
    settings_path: String,
    database_path: String,
    record_count: usize,
}

pub(super) unsafe fn show(
    parent: HWND,
    config: OptionsConfig,
    settings_path: &Path,
    database_path: &Path,
    record_count: usize,
    apply_message: u32,
) {
    register_class();
    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        return;
    }

    let mut parent_rect = RECT::default();
    unsafe { GetWindowRect(parent, &mut parent_rect) };
    let width = 650;
    let height = 460;
    let x = parent_rect.left + ((parent_rect.right - parent_rect.left - width) / 2).max(0);
    let y = parent_rect.top + ((parent_rect.bottom - parent_rect.top - height) / 2).max(0);
    let create = Box::into_raw(Box::new(CreateState {
        parent,
        apply_message,
        config,
        settings_path: settings_path.display().to_string(),
        database_path: database_path.display().to_string(),
        record_count,
    }));
    let window = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            wide_null(CLASS_NAME).as_ptr(),
            wide_null(TITLE).as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
            x,
            y,
            width,
            height,
            parent,
            null_mut(),
            instance,
            create.cast(),
        )
    };
    unsafe { drop(Box::from_raw(create)) };
    if window.is_null() {
        return;
    }

    unsafe {
        EnableWindow(parent, 0);
        ShowWindow(window, SW_SHOW);
        SetFocus((*window_state(window)).tree);
    }
    let mut message = MSG::default();
    while unsafe { IsWindow(window) } != 0 {
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result <= 0 {
            if result == 0 {
                unsafe { PostQuitMessage(message.wParam as i32) };
            }
            break;
        }
        if unsafe { IsDialogMessageW(window, &message) } == 0 {
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

fn register_class() {
    REGISTER_CLASS.call_once(|| {
        let class_name = wide_null(CLASS_NAME);
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(options_proc),
            hInstance: unsafe { GetModuleHandleW(null()) },
            hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
            hbrBackground: (COLOR_WINDOW + 1) as usize as _,
            lpszClassName: class_name.as_ptr(),
            ..WNDCLASSEXW::default()
        };
        unsafe { RegisterClassExW(&class) };
    });
}

unsafe extern "system" fn options_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    if message == WM_NCCREATE {
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    }
    if message == WM_CREATE {
        let create = unsafe {
            &*(lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW)
        };
        let args = unsafe { &*(create.lpCreateParams as *const CreateState) };
        return match unsafe { create_controls(window, args) } {
            Some(options) => {
                unsafe {
                    SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(options) as isize)
                };
                1
            }
            None => 0,
        };
    }

    let state_ptr = unsafe { window_state(window) };
    match message {
        WM_SIZE if !state_ptr.is_null() => {
            unsafe { layout(window, &*state_ptr) };
            0
        }
        WM_NOTIFY if !state_ptr.is_null() => {
            let notification = unsafe { &*(lparam as *const NMTREEVIEWW) };
            if notification.hdr.hwndFrom == unsafe { (*state_ptr).tree }
                && notification.hdr.code == TVN_SELCHANGEDW
            {
                unsafe { show_page(&*state_ptr, notification.itemNew.lParam as usize) };
            }
            0
        }
        WM_COMMAND if !state_ptr.is_null() => {
            let command = wparam & 0xffff;
            let notification = (wparam >> 16) as u32;
            let state = unsafe { &mut *state_ptr };
            match command {
                ID_OK => unsafe {
                    apply_options(state);
                    DestroyWindow(window);
                },
                ID_CANCEL => unsafe {
                    DestroyWindow(window);
                },
                ID_APPLY => unsafe { apply_options(state) },
                ID_FORCE_REBUILD => {
                    state.force_rebuild_pending = true;
                    unsafe {
                        SetWindowTextW(
                            state.force_rebuild,
                            wide_null("Rebuild requested").as_ptr(),
                        );
                        EnableWindow(state.force_rebuild, 0);
                        EnableWindow(state.apply, 1);
                    }
                }
                ID_STATUS_BAR | ID_SELECTED_STATUS | ID_SEARCH_AS_TYPE | ID_MATCH_CASE
                | ID_MATCH_WHOLE_WORD | ID_MATCH_PATH
                    if notification == BN_CLICKED =>
                unsafe {
                    EnableWindow(state.apply, 1);
                },
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(window) };
            0
        }
        WM_DESTROY if !state_ptr.is_null() => {
            let state = unsafe { Box::from_raw(state_ptr) };
            unsafe {
                EnableWindow(state.parent, 1);
                SetFocus(state.parent);
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe fn create_controls(window: HWND, create: &CreateState) -> Option<Box<OptionsState>> {
    let instance = unsafe { GetModuleHandleW(null()) };
    let tree = control(
        window,
        WC_TREEVIEWW,
        "",
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WS_BORDER
            | TVS_HASBUTTONS
            | TVS_HASLINES
            | TVS_LINESATROOT
            | TVS_SHOWSELALWAYS,
        ID_TREE,
        instance,
    )?;
    let title = control(
        window,
        windows_sys::core::w!("STATIC"),
        "General",
        WS_CHILD | WS_VISIBLE,
        0,
        instance,
    )?;

    let mut page_controls = Vec::new();
    let general_text = format!(
        "EveRusthing settings\r\n{}\r\n\r\nEverything 1.4 compatible options.",
        create.settings_path
    );
    add_static(
        window,
        &mut page_controls,
        PAGE_GENERAL,
        &general_text,
        instance,
    )?;

    let status_bar = add_checkbox(
        window,
        &mut page_controls,
        PAGE_VIEW,
        "Show status bar",
        ID_STATUS_BAR,
        instance,
    )?;
    let selected_status = add_checkbox(
        window,
        &mut page_controls,
        PAGE_VIEW,
        "Show selected item in status bar",
        ID_SELECTED_STATUS,
        instance,
    )?;

    let search_as_type = add_checkbox(
        window,
        &mut page_controls,
        PAGE_SEARCH,
        "Search as you type",
        ID_SEARCH_AS_TYPE,
        instance,
    )?;
    let match_case = add_checkbox(
        window,
        &mut page_controls,
        PAGE_SEARCH,
        "Match case when opening a new search window",
        ID_MATCH_CASE,
        instance,
    )?;
    let match_whole_word = add_checkbox(
        window,
        &mut page_controls,
        PAGE_SEARCH,
        "Match whole word when opening a new search window",
        ID_MATCH_WHOLE_WORD,
        instance,
    )?;
    let match_path = add_checkbox(
        window,
        &mut page_controls,
        PAGE_SEARCH,
        "Match path when opening a new search window",
        ID_MATCH_PATH,
        instance,
    )?;

    let database_text = format!(
        "Database\r\n{}\r\n\r\nIndexed objects: {}",
        create.database_path, create.record_count
    );
    add_static(
        window,
        &mut page_controls,
        PAGE_INDEXES,
        &database_text,
        instance,
    )?;
    let force_rebuild = add_button(
        window,
        &mut page_controls,
        PAGE_INDEXES,
        "Force Rebuild",
        ID_FORCE_REBUILD,
        instance,
    )?;

    let volumes = crate::ntfs::discover_ntfs_volumes()
        .map(|roots| {
            roots
                .into_iter()
                .map(|root| root.to_string())
                .collect::<Vec<_>>()
                .join("\r\n")
        })
        .unwrap_or_else(|error| format!("Unable to enumerate volumes: {error}"));
    add_static(
        window,
        &mut page_controls,
        PAGE_NTFS,
        &format!("Local NTFS volumes:\r\n\r\n{volumes}"),
        instance,
    )?;

    let ok = push_button(window, "OK", ID_OK, true, instance)?;
    let cancel = push_button(window, "Cancel", ID_CANCEL, false, instance)?;
    let apply = push_button(window, "Apply", ID_APPLY, false, instance)?;
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    for child in [tree, title, ok, cancel, apply]
        .into_iter()
        .chain(page_controls.iter().map(|control| control.window))
    {
        unsafe { SendMessageW(child, WM_SETFONT, font as usize, 1) };
    }
    unsafe {
        set_checked(status_bar, create.config.show_status_bar);
        set_checked(
            selected_status,
            create.config.show_selected_item_in_statusbar,
        );
        set_checked(search_as_type, create.config.search_as_you_type);
        set_checked(match_case, create.config.query.match_case);
        set_checked(match_whole_word, create.config.query.match_whole_word);
        set_checked(match_path, create.config.query.match_path);
        EnableWindow(apply, 0);
    }

    let general = unsafe { insert_tree_item(tree, TVI_ROOT, "General", PAGE_GENERAL) };
    unsafe {
        insert_tree_item(tree, TVI_ROOT, "View", PAGE_VIEW);
        insert_tree_item(tree, TVI_ROOT, "Search", PAGE_SEARCH);
    }
    let indexes = unsafe { insert_tree_item(tree, TVI_ROOT, "Indexes", PAGE_INDEXES) };
    unsafe {
        insert_tree_item(tree, indexes, "NTFS", PAGE_NTFS);
        SendMessageW(tree, TVM_EXPAND, TVE_EXPAND as usize, indexes as isize);
        SendMessageW(tree, TVM_SELECTITEM, TVGN_CARET as usize, general as isize);
    }

    let options = Box::new(OptionsState {
        parent: create.parent,
        apply_message: create.apply_message,
        tree,
        title,
        page_controls,
        status_bar,
        selected_status,
        search_as_type,
        match_case,
        match_whole_word,
        match_path,
        force_rebuild,
        apply,
        force_rebuild_pending: false,
    });
    unsafe {
        layout(window, &options);
        show_page(&options, PAGE_GENERAL);
    }
    Some(options)
}

unsafe fn apply_options(state: &mut OptionsState) {
    let applied = Box::new(AppliedOptions {
        config: OptionsConfig {
            show_status_bar: unsafe { is_checked(state.status_bar) },
            show_selected_item_in_statusbar: unsafe { is_checked(state.selected_status) },
            search_as_you_type: unsafe { is_checked(state.search_as_type) },
            query: QueryOptions {
                match_case: unsafe { is_checked(state.match_case) },
                match_whole_word: unsafe { is_checked(state.match_whole_word) },
                match_path: unsafe { is_checked(state.match_path) },
            },
        },
        force_rebuild: state.force_rebuild_pending,
    });
    unsafe {
        SendMessageW(
            state.parent,
            state.apply_message,
            0,
            Box::into_raw(applied) as isize,
        );
        EnableWindow(state.apply, 0);
    }
    state.force_rebuild_pending = false;
}

unsafe fn show_page(state: &OptionsState, page: usize) {
    let Some(title) = PAGE_TITLES.get(page) else {
        return;
    };
    unsafe { SetWindowTextW(state.title, wide_null(title).as_ptr()) };
    for control in &state.page_controls {
        unsafe {
            ShowWindow(
                control.window,
                if control.page == page {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            )
        };
    }
}

unsafe fn layout(window: HWND, state: &OptionsState) {
    let mut rect = RECT::default();
    unsafe { GetClientRect(window, &mut rect) };
    let width = rect.right.max(0);
    let height = rect.bottom.max(0);
    let button_y = height - 38;
    unsafe {
        MoveWindow(state.tree, 12, 12, 170, (height - 62).max(100), 1);
        MoveWindow(state.title, 200, 16, (width - 214).max(100), 24, 1);
        MoveWindow(state.apply, width - 90, button_y, 78, 24, 1);
        MoveWindow(
            GetDlgItem(window, ID_OK as i32),
            width - 258,
            button_y,
            78,
            24,
            1,
        );
        MoveWindow(
            GetDlgItem(window, ID_CANCEL as i32),
            width - 174,
            button_y,
            78,
            24,
            1,
        );
    }
    let mut offsets = [56; 5];
    for control in &state.page_controls {
        let y = offsets[control.page];
        unsafe {
            MoveWindow(
                control.window,
                215,
                y,
                (width - 230).max(100),
                control.height,
                1,
            )
        };
        offsets[control.page] += control.height + 8;
    }
}

fn add_static(
    parent: HWND,
    controls: &mut Vec<PageControl>,
    page: usize,
    text: &str,
    instance: HINSTANCE,
) -> Option<HWND> {
    let window = control(
        parent,
        windows_sys::core::w!("STATIC"),
        text,
        WS_CHILD,
        0,
        instance,
    )?;
    controls.push(PageControl {
        window,
        page,
        height: 82,
    });
    Some(window)
}

fn add_checkbox(
    parent: HWND,
    controls: &mut Vec<PageControl>,
    page: usize,
    text: &str,
    id: usize,
    instance: HINSTANCE,
) -> Option<HWND> {
    let window = control(
        parent,
        windows_sys::core::w!("BUTTON"),
        text,
        WS_CHILD | WS_TABSTOP | WS_GROUP | BS_AUTOCHECKBOX as u32,
        id,
        instance,
    )?;
    controls.push(PageControl {
        window,
        page,
        height: 24,
    });
    Some(window)
}

fn add_button(
    parent: HWND,
    controls: &mut Vec<PageControl>,
    page: usize,
    text: &str,
    id: usize,
    instance: HINSTANCE,
) -> Option<HWND> {
    let window = push_button(parent, text, id, false, instance)?;
    controls.push(PageControl {
        window,
        page,
        height: 24,
    });
    Some(window)
}

fn push_button(
    parent: HWND,
    text: &str,
    id: usize,
    default: bool,
    instance: HINSTANCE,
) -> Option<HWND> {
    control(
        parent,
        windows_sys::core::w!("BUTTON"),
        text,
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | if default {
                BS_DEFPUSHBUTTON as u32
            } else {
                BS_PUSHBUTTON as u32
            },
        id,
        instance,
    )
}

fn control(
    parent: HWND,
    class_name: *const u16,
    text: &str,
    style: u32,
    id: usize,
    instance: HINSTANCE,
) -> Option<HWND> {
    let window = unsafe {
        CreateWindowExW(
            0,
            class_name,
            wide_null(text).as_ptr(),
            style,
            0,
            0,
            0,
            0,
            parent,
            id as _,
            instance,
            null(),
        )
    };
    (!window.is_null()).then_some(window)
}

unsafe fn insert_tree_item(tree: HWND, parent: HTREEITEM, text: &str, page: usize) -> HTREEITEM {
    let mut text = wide_null(text);
    let item = windows_sys::Win32::UI::Controls::TVITEMW {
        mask: TVIF_TEXT | TVIF_PARAM,
        pszText: text.as_mut_ptr(),
        cchTextMax: text.len() as i32,
        lParam: page as isize,
        ..Default::default()
    };
    let insert = TVINSERTSTRUCTW {
        hParent: parent,
        hInsertAfter: TVI_LAST,
        Anonymous: windows_sys::Win32::UI::Controls::TVINSERTSTRUCTW_0 { item },
    };
    unsafe { SendMessageW(tree, TVM_INSERTITEMW, 0, &insert as *const _ as isize) as HTREEITEM }
}

unsafe fn set_checked(window: HWND, checked: bool) {
    unsafe {
        SendMessageW(
            window,
            BM_SETCHECK,
            if checked { BST_CHECKED as usize } else { 0 },
            0,
        )
    };
}

unsafe fn is_checked(window: HWND) -> bool {
    unsafe { SendMessageW(window, BM_GETCHECK, 0, 0) as u32 == BST_CHECKED }
}

unsafe fn window_state(window: HWND) -> *mut OptionsState {
    unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) as *mut OptionsState }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}
