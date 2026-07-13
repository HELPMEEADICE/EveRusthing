use std::env;
use std::fs;
use std::mem::size_of;
use std::path::PathBuf;
use std::process::Command;
use std::ptr::{copy_nonoverlapping, null, null_mut};
use std::sync::Arc;
use std::thread;

use windows_sys::Win32::Foundation::{
    FILETIME, GlobalFree, HWND, LPARAM, POINT, RECT, SYSTEMTIME, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject, UpdateWindow,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Controls::{
    ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES, ICC_TREEVIEW_CLASSES, INITCOMMONCONTROLSEX,
    InitCommonControlsEx, LVCF_FMT, LVCF_TEXT, LVCF_WIDTH, LVCFMT_LEFT, LVCFMT_RIGHT, LVCOLUMNW,
    LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_GETNEXTITEM, LVM_GETSELECTEDCOUNT,
    LVM_INSERTCOLUMNW, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMCOUNT, LVM_SETITEMSTATE,
    LVN_COLUMNCLICK, LVN_GETDISPINFOW, LVN_ITEMCHANGED, LVNI_SELECTED, LVS_EX_DOUBLEBUFFER,
    LVS_EX_FULLROWSELECT, LVS_EX_LABELTIP, LVS_OWNERDATA, LVS_REPORT, LVS_SHOWSELALWAYS, NM_DBLCLK,
    NM_RCLICK, NM_RETURN, NMHDR, NMLISTVIEW, NMLVDISPINFOW, SB_SETTEXTW, SBARS_SIZEGRIP,
    STATUSCLASSNAMEW, WC_LISTVIEWW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_DOWN, VK_ESCAPE, VK_RETURN};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CheckMenuItem, CreateMenu, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW, EN_CHANGE,
    ES_AUTOHSCROLL, GWLP_USERDATA, GetClientRect, GetCursorPos, GetMessageW, GetWindowLongPtrW,
    GetWindowPlacement, GetWindowTextLengthW, GetWindowTextW, IDC_ARROW, LoadCursorW,
    MB_ICONINFORMATION, MB_OK, MF_BYCOMMAND, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MF_UNCHECKED, MSG, MessageBoxW, MoveWindow, PostMessageW, PostQuitMessage, RegisterClassExW,
    SW_SHOWMAXIMIZED, SW_SHOWNORMAL, SendMessageW, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_KEYDOWN, WM_NOTIFY, WM_SETFOCUS, WM_SETFONT, WM_SIZE, WNDCLASSEXW, WS_BORDER,
    WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
};

use crate::FileRecord;
use crate::database::{self, VolumeCheckpoint};
use crate::index::SharedIndex;
use crate::monitor::{self, MonitorEvent};
use crate::query::{Query, QueryOptions};
use crate::result_sort::ResultSorter;

mod options_dialog;

const CLASS_NAME: &str = "EVERUSTHING";
const WINDOW_TITLE: &str = "EveRusthing";
const ID_SEARCH_EDIT: usize = 100;
const ID_RESULT_LIST: usize = 101;

const CMD_NEW_WINDOW: usize = 1000;
const CMD_CLOSE: usize = 1001;
const CMD_EXIT: usize = 1002;
const CMD_OPEN: usize = 1010;
const CMD_OPEN_PATH: usize = 1011;
const CMD_COPY_FULL_PATH: usize = 1012;
const CMD_SELECT_ALL: usize = 1013;
const CMD_STATUS_BAR: usize = 1020;
const CMD_MATCH_CASE: usize = 1030;
const CMD_MATCH_WHOLE_WORD: usize = 1031;
const CMD_MATCH_PATH: usize = 1032;
const CMD_OPTIONS: usize = 1040;
const CMD_BOOKMARKS: usize = 1041;
const CMD_HELP: usize = 1050;
const CMD_ABOUT: usize = 1051;
const WM_INDEX_LOADED: u32 = WM_APP + 1;
const WM_OPTIONS_APPLIED: u32 = WM_APP + 2;
const WM_INDEX_UPDATED: u32 = WM_APP + 3;
const WM_SORT_READY: u32 = WM_APP + 4;
const CF_UNICODETEXT: u32 = 13;

#[derive(Clone, Debug, Eq, PartialEq)]
struct GuiSettings {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    maximized: bool,
    show_status_bar: bool,
    show_selected_item_in_statusbar: bool,
    search_as_you_type: bool,
    options: QueryOptions,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            x: CW_USEDEFAULT,
            y: CW_USEDEFAULT,
            width: 860,
            height: 560,
            maximized: false,
            show_status_bar: true,
            show_selected_item_in_statusbar: true,
            search_as_you_type: true,
            options: QueryOptions::default(),
        }
    }
}

impl GuiSettings {
    fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "window_x" => settings.x = value.trim().parse().unwrap_or(settings.x),
                "window_y" => settings.y = value.trim().parse().unwrap_or(settings.y),
                "window_width" => settings.width = value.trim().parse().unwrap_or(settings.width),
                "window_height" => {
                    settings.height = value.trim().parse().unwrap_or(settings.height)
                }
                "window_maximized" => settings.maximized = parse_bool(value),
                "show_status_bar" => settings.show_status_bar = parse_bool(value),
                "show_selected_item_in_statusbar" => {
                    settings.show_selected_item_in_statusbar = parse_bool(value)
                }
                "search_as_you_type" => settings.search_as_you_type = parse_bool(value),
                "match_case" => settings.options.match_case = parse_bool(value),
                "match_whole_word" => settings.options.match_whole_word = parse_bool(value),
                "match_path" => settings.options.match_path = parse_bool(value),
                _ => {}
            }
        }
        settings.width = settings.width.max(480);
        settings.height = settings.height.max(320);
        settings
    }

    fn serialize(&self) -> String {
        format!(
            "[Everything]\r\nwindow_x={}\r\nwindow_y={}\r\nwindow_width={}\r\nwindow_height={}\r\nwindow_maximized={}\r\nshow_status_bar={}\r\nshow_selected_item_in_statusbar={}\r\nsearch_as_you_type={}\r\nmatch_case={}\r\nmatch_whole_word={}\r\nmatch_path={}\r\n",
            self.x,
            self.y,
            self.width,
            self.height,
            u8::from(self.maximized),
            u8::from(self.show_status_bar),
            u8::from(self.show_selected_item_in_statusbar),
            u8::from(self.search_as_you_type),
            u8::from(self.options.match_case),
            u8::from(self.options.match_whole_word),
            u8::from(self.options.match_path),
        )
    }
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

struct LoadResult {
    loaded: Result<LoadedIndex, String>,
}

struct LoadedIndex {
    records: Vec<FileRecord>,
    index: SharedIndex,
    checkpoints: Vec<VolumeCheckpoint>,
}

struct MonitorResult {
    generation: u64,
    records: Result<Vec<FileRecord>, String>,
}

struct SortResult {
    records_generation: u64,
    column: usize,
    order: Vec<u32>,
}

struct AppState {
    search: HWND,
    list: HWND,
    status: HWND,
    records: Arc<Vec<FileRecord>>,
    visible: Vec<u32>,
    last_search: Option<String>,
    last_options: QueryOptions,
    options: QueryOptions,
    sort_column: usize,
    sort_ascending: bool,
    result_sorter: ResultSorter,
    records_generation: u64,
    sort_in_progress: bool,
    show_status_bar: bool,
    show_selected_item_in_statusbar: bool,
    search_as_you_type: bool,
    loading: bool,
    settings_path: PathBuf,
    pipe_name: String,
    use_database: bool,
    monitor: Option<monitor::Monitor>,
    monitor_generation: u64,
}

pub fn run(
    pipe_name: &str,
    initial_search: &str,
    initial_options: QueryOptions,
    use_database: bool,
    force_reindex: bool,
) -> Result<(), String> {
    let settings_path = settings_path();
    let mut settings = fs::read_to_string(&settings_path)
        .map(|text| GuiSettings::parse(&text))
        .unwrap_or_default();
    if initial_options != QueryOptions::default() {
        settings.options = initial_options;
    }

    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        return Err("get GUI module handle failed".into());
    }
    let controls = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES | ICC_TREEVIEW_CLASSES,
    };
    unsafe { InitCommonControlsEx(&controls) };

    let class_name = wide_null(CLASS_NAME);
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: (COLOR_WINDOW + 1) as usize as _,
        lpszClassName: class_name.as_ptr(),
        ..WNDCLASSEXW::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        return Err("register EveRusthing window class failed".into());
    }

    let title = wide_null(WINDOW_TITLE);
    let window = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            settings.x,
            settings.y,
            settings.width,
            settings.height,
            null_mut(),
            create_main_menu(),
            instance,
            null(),
        )
    };
    if window.is_null() {
        return Err("create EveRusthing main window failed".into());
    }

    let search = create_child(
        window,
        windows_sys::core::w!("EDIT"),
        initial_search,
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
        ID_SEARCH_EDIT,
        instance,
    )?;
    let list = create_child_raw(
        window,
        WC_LISTVIEWW,
        null(),
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WS_BORDER
            | LVS_REPORT
            | LVS_OWNERDATA
            | LVS_SHOWSELALWAYS,
        ID_RESULT_LIST,
        instance,
    )?;
    let status = create_child_raw(
        window,
        STATUSCLASSNAMEW,
        null(),
        WS_CHILD | WS_VISIBLE | SBARS_SIZEGRIP,
        0,
        instance,
    )?;

    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    unsafe {
        SendMessageW(search, WM_SETFONT, font as usize, 1);
        SendMessageW(list, WM_SETFONT, font as usize, 1);
        SendMessageW(status, WM_SETFONT, font as usize, 1);
        let styles = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP;
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            styles as usize,
            styles as isize,
        );
    }
    unsafe { insert_columns(list) };

    let state = Box::new(AppState {
        search,
        list,
        status,
        records: Arc::new(Vec::new()),
        visible: Vec::new(),
        last_search: None,
        last_options: settings.options,
        options: settings.options,
        sort_column: 0,
        sort_ascending: true,
        result_sorter: ResultSorter::default(),
        records_generation: 0,
        sort_in_progress: false,
        show_status_bar: settings.show_status_bar,
        show_selected_item_in_statusbar: settings.show_selected_item_in_statusbar,
        search_as_you_type: settings.search_as_you_type,
        loading: true,
        settings_path,
        pipe_name: pipe_name.to_owned(),
        use_database,
        monitor: None,
        monitor_generation: 0,
    });
    let state = Box::into_raw(state);
    unsafe {
        SetWindowLongPtrW(window, GWLP_USERDATA, state as isize);
        sync_menu_checks(window, &*state);
        layout(window, &*state);
        set_status(&*state, "Loading database...");
        ShowWindow(
            window,
            if settings.maximized {
                SW_SHOWMAXIMIZED
            } else {
                SW_SHOWNORMAL
            },
        );
        UpdateWindow(window);
        SetFocus(search);
    }
    start_index_load(window, pipe_name.to_owned(), use_database, force_reindex);

    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result == 0 {
            break;
        }
        if result == -1 {
            return Err("read Windows GUI message failed".into());
        }
        if unsafe { handle_search_key(&mut *state, &message) } {
            continue;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    let state_ptr = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut AppState;
    match message {
        WM_SIZE if !state_ptr.is_null() => {
            unsafe { layout(window, &*state_ptr) };
            0
        }
        WM_SETFOCUS if !state_ptr.is_null() => {
            unsafe { SetFocus((*state_ptr).search) };
            0
        }
        WM_COMMAND if !state_ptr.is_null() => {
            let command = wparam & 0xffff;
            let notification = (wparam >> 16) as u32;
            let state = unsafe { &mut *state_ptr };
            if command == ID_SEARCH_EDIT && notification == EN_CHANGE && state.search_as_you_type {
                unsafe { refresh_results(state) };
            } else {
                unsafe { handle_command(window, state, command) };
            }
            0
        }
        WM_NOTIFY if !state_ptr.is_null() => unsafe {
            handle_notification(window, &mut *state_ptr, lparam)
        },
        WM_INDEX_LOADED if !state_ptr.is_null() => {
            let result = unsafe { Box::from_raw(lparam as *mut LoadResult) };
            let state = unsafe { &mut *state_ptr };
            state.loading = false;
            match result.loaded {
                Ok(loaded) => {
                    let LoadedIndex {
                        records,
                        index,
                        checkpoints,
                    } = loaded;
                    replace_records(state, records);
                    if unsafe { window_text(state.search) }.is_empty()
                        && state.sort_column == 0
                        && state.sort_ascending
                    {
                        state.visible = (0..state.records.len() as u32).collect();
                        state.last_search = Some(String::new());
                        state.last_options = state.options;
                        unsafe {
                            SendMessageW(state.list, LVM_SETITEMCOUNT, state.visible.len(), 0);
                            set_object_status(state, state.visible.len());
                        }
                    } else {
                        unsafe { refresh_results(state) };
                    }
                    request_background_sort(window, state);
                    state.monitor_generation = state.monitor_generation.wrapping_add(1);
                    state.monitor = Some(start_monitor(
                        window,
                        index,
                        checkpoints,
                        state.pipe_name.clone(),
                        state.use_database,
                        state.monitor_generation,
                    ));
                }
                Err(error) => {
                    replace_records(state, Vec::new());
                    state.visible.clear();
                    unsafe {
                        SendMessageW(state.list, LVM_SETITEMCOUNT, 0, 0);
                        set_status(state, &error);
                    }
                }
            }
            0
        }
        WM_INDEX_UPDATED if !state_ptr.is_null() => {
            let result = unsafe { Box::from_raw(lparam as *mut MonitorResult) };
            let state = unsafe { &mut *state_ptr };
            if result.generation != state.monitor_generation {
                return 0;
            }
            match result.records {
                Ok(records) => {
                    replace_records(state, records);
                    state.last_search = None;
                    unsafe { refresh_results(state) };
                    request_background_sort(window, state);
                }
                Err(error) => unsafe { set_status(state, &error) },
            }
            0
        }
        WM_SORT_READY if !state_ptr.is_null() => {
            let result = unsafe { Box::from_raw(lparam as *mut SortResult) };
            let state = unsafe { &mut *state_ptr };
            state.sort_in_progress = false;
            if result.records_generation == state.records_generation
                && result.column == state.sort_column
            {
                state.result_sorter.install(result.column, result.order);
                sort_results(state, false);
                unsafe {
                    SendMessageW(state.list, LVM_SETITEMCOUNT, state.visible.len(), 0);
                    set_object_status(state, state.visible.len());
                }
            }
            request_background_sort(window, state);
            0
        }
        WM_OPTIONS_APPLIED if !state_ptr.is_null() => {
            let applied = unsafe { Box::from_raw(lparam as *mut options_dialog::AppliedOptions) };
            let state = unsafe { &mut *state_ptr };
            state.show_status_bar = applied.config.show_status_bar;
            state.show_selected_item_in_statusbar = applied.config.show_selected_item_in_statusbar;
            state.search_as_you_type = applied.config.search_as_you_type;
            state.options = applied.config.query;
            unsafe {
                sync_menu_checks(window, state);
                layout(window, state);
                refresh_results(state);
                save_settings(window, state);
            }
            if applied.force_rebuild {
                state.monitor_generation = state.monitor_generation.wrapping_add(1);
                state.monitor = None;
                state.loading = true;
                replace_records(state, Vec::new());
                state.visible.clear();
                state.last_search = None;
                unsafe {
                    SendMessageW(state.list, LVM_SETITEMCOUNT, 0, 0);
                    set_status(state, "Rebuilding database...");
                }
                start_index_load(window, state.pipe_name.clone(), state.use_database, true);
            }
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(window) };
            0
        }
        WM_DESTROY => {
            if !state_ptr.is_null() {
                let state = unsafe { Box::from_raw(state_ptr) };
                save_settings(window, &state);
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
            }
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe fn handle_notification(window: HWND, state: &mut AppState, lparam: LPARAM) -> isize {
    let header = unsafe { &*(lparam as *const NMHDR) };
    if header.hwndFrom != state.list {
        return 0;
    }
    match header.code {
        LVN_GETDISPINFOW => {
            let info = unsafe { &mut *(lparam as *mut NMLVDISPINFOW) };
            if info.item.mask & LVIF_TEXT != 0 && info.item.iItem >= 0 {
                let row = info.item.iItem as usize;
                if let Some(record) = state
                    .visible
                    .get(row)
                    .and_then(|index| state.records.get(*index as usize))
                {
                    let text = column_text(record, info.item.iSubItem.max(0) as usize);
                    unsafe { copy_wide_to_buffer(&text, info.item.pszText, info.item.cchTextMax) };
                }
            }
        }
        LVN_COLUMNCLICK => {
            let info = unsafe { &*(lparam as *const NMLISTVIEW) };
            let column = info.iSubItem.max(0) as usize;
            if state.sort_column == column {
                state.sort_ascending = !state.sort_ascending;
                state.visible.reverse();
            } else {
                state.sort_column = column;
                state.sort_ascending = true;
                if !sort_results(state, false) {
                    request_background_sort(window, state);
                }
            }
            unsafe { SendMessageW(state.list, LVM_SETITEMCOUNT, state.visible.len(), 0) };
        }
        LVN_ITEMCHANGED => unsafe { update_selection_status(state) },
        NM_DBLCLK | NM_RETURN => unsafe { open_selected(window, state, false) },
        NM_RCLICK => unsafe { show_result_menu(window, state) },
        _ => {}
    }
    0
}

unsafe fn handle_command(window: HWND, state: &mut AppState, command: usize) {
    match command {
        CMD_NEW_WINDOW => {
            if let Ok(executable) = env::current_exe() {
                let _ = Command::new(executable).spawn();
            }
        }
        CMD_CLOSE | CMD_EXIT => unsafe {
            DestroyWindow(window);
        },
        CMD_OPEN => unsafe { open_selected(window, state, false) },
        CMD_OPEN_PATH => unsafe { open_selected(window, state, true) },
        CMD_COPY_FULL_PATH => unsafe { copy_selected_paths(window, state) },
        CMD_SELECT_ALL => unsafe { select_all(state) },
        CMD_STATUS_BAR => {
            state.show_status_bar = !state.show_status_bar;
            unsafe {
                sync_menu_checks(window, state);
                layout(window, state);
            }
        }
        CMD_MATCH_CASE => {
            state.options.match_case = !state.options.match_case;
            unsafe { option_changed(window, state) };
        }
        CMD_MATCH_WHOLE_WORD => {
            state.options.match_whole_word = !state.options.match_whole_word;
            unsafe { option_changed(window, state) };
        }
        CMD_MATCH_PATH => {
            state.options.match_path = !state.options.match_path;
            unsafe { option_changed(window, state) };
        }
        CMD_OPTIONS => unsafe {
            let config = options_dialog::OptionsConfig {
                show_status_bar: state.show_status_bar,
                show_selected_item_in_statusbar: state.show_selected_item_in_statusbar,
                search_as_you_type: state.search_as_you_type,
                query: state.options,
            };
            options_dialog::show(
                window,
                config,
                &state.settings_path,
                &database::default_path(),
                state.records.len(),
                WM_OPTIONS_APPLIED,
            );
        },
        CMD_BOOKMARKS => unsafe {
            show_message(
                window,
                "Bookmarks",
                "Bookmark storage will be enabled with the Everything bookmark database format.",
            )
        },
        CMD_HELP => unsafe {
            show_message(
                window,
                "Everything Help",
                "Type a name in the search box.\r\n\r\nOperators: space = AND, | = OR, ! = NOT, < > = grouping.\r\nWildcards: * and ?.\r\nPress Enter or double-click to open a result.",
            )
        },
        CMD_ABOUT => unsafe {
            show_message(
                window,
                "About EveRusthing",
                "EveRusthing 0.1.0\r\nRust x64 reimplementation of Everything 1.4.1.1032",
            )
        },
        _ => {}
    }
}

unsafe fn option_changed(window: HWND, state: &mut AppState) {
    unsafe {
        sync_menu_checks(window, state);
        refresh_results(state);
    }
}

unsafe fn refresh_results(state: &mut AppState) {
    if state.loading {
        return;
    }
    let search = unsafe { window_text(state.search) };
    match Query::parse(&search, state.options) {
        Ok(query) => {
            let refined = state.last_options == state.options
                && !state.options.match_whole_word
                && state.last_search.as_deref().is_some_and(|previous| {
                    search.starts_with(previous)
                        && is_simple_search(previous)
                        && is_simple_search(&search)
                });
            if refined {
                state
                    .visible
                    .retain(|index| query.matches(&state.records[*index as usize]));
            } else {
                state.visible.clear();
                if state.sort_column != 0
                    && let Some(order) = state.result_sorter.ordered_indices(state.sort_column)
                {
                    state.visible.extend(
                        order
                            .iter()
                            .copied()
                            .filter(|index| query.matches(&state.records[*index as usize])),
                    );
                    if !state.sort_ascending {
                        state.visible.reverse();
                    }
                } else {
                    state
                        .visible
                        .extend(
                            state
                                .records
                                .iter()
                                .enumerate()
                                .filter_map(|(index, record)| {
                                    query.matches(record).then_some(index as u32)
                                }),
                        );
                    sort_results(state, true);
                }
            }
            state.last_search = Some(search);
            state.last_options = state.options;
            unsafe {
                SendMessageW(state.list, LVM_SETITEMCOUNT, state.visible.len(), 0);
                set_object_status(state, state.visible.len());
            }
        }
        Err(error) => {
            state.visible.clear();
            state.last_search = None;
            unsafe {
                SendMessageW(state.list, LVM_SETITEMCOUNT, 0, 0);
                set_status(state, &format!("Invalid search: {error}"));
            }
        }
    }
}

fn sort_results(state: &mut AppState, already_default_sorted: bool) -> bool {
    if state.sort_column != 0
        && state
            .result_sorter
            .ordered_indices(state.sort_column)
            .is_none()
    {
        return false;
    }
    state.result_sorter.sort(
        &state.records,
        &mut state.visible,
        state.sort_column,
        state.sort_ascending,
        already_default_sorted,
    );
    true
}

fn replace_records(state: &mut AppState, records: Vec<FileRecord>) {
    state.records = Arc::new(records);
    state.records_generation = state.records_generation.wrapping_add(1);
    state.result_sorter.invalidate();
}

fn request_background_sort(window: HWND, state: &mut AppState) {
    if state.loading
        || state.sort_column == 0
        || state.sort_in_progress
        || state
            .result_sorter
            .ordered_indices(state.sort_column)
            .is_some()
    {
        return;
    }

    state.sort_in_progress = true;
    let records = Arc::clone(&state.records);
    let records_generation = state.records_generation;
    let column = state.sort_column;
    let order_storage = state.result_sorter.take_order_storage();
    let window_value = window as usize;
    unsafe { set_status(state, "Sorting...") };
    thread::spawn(move || {
        let result = Box::new(SortResult {
            records_generation,
            column,
            order: ResultSorter::build_order_reusing(&records, column, order_storage),
        });
        let result = Box::into_raw(result);
        if unsafe { PostMessageW(window_value as HWND, WM_SORT_READY, 0, result as isize) } == 0 {
            unsafe { drop(Box::from_raw(result)) };
        }
    });
}

fn is_simple_search(search: &str) -> bool {
    !search.chars().any(|character| {
        character.is_whitespace()
            || matches!(
                character,
                '|' | '!' | '<' | '>' | '"' | '*' | '?' | ':' | '\\' | '/'
            )
    })
}

unsafe fn layout(window: HWND, state: &AppState) {
    let mut client = RECT::default();
    unsafe { GetClientRect(window, &mut client) };
    let width = (client.right - client.left).max(0);
    let height = (client.bottom - client.top).max(0);
    let search_height = 24;
    let status_height = if state.show_status_bar { 22 } else { 0 };
    unsafe {
        MoveWindow(state.search, 0, 0, width, search_height, 1);
        MoveWindow(
            state.list,
            0,
            search_height,
            width,
            (height - search_height - status_height).max(0),
            1,
        );
        MoveWindow(
            state.status,
            0,
            (height - status_height).max(0),
            width,
            status_height,
            1,
        );
        windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
            state.status,
            if state.show_status_bar { 5 } else { 0 },
        );
    }
}

unsafe fn handle_search_key(state: &mut AppState, message: &MSG) -> bool {
    if message.hwnd != state.search || message.message != WM_KEYDOWN {
        return false;
    }
    match message.wParam as u16 {
        VK_ESCAPE => {
            unsafe { SetWindowTextW(state.search, wide_null("").as_ptr()) };
            if !state.search_as_you_type {
                unsafe { refresh_results(state) };
            }
            true
        }
        VK_RETURN => {
            if !state.search_as_you_type {
                unsafe { refresh_results(state) };
            }
            if state.visible.is_empty() {
                return true;
            }
            let mut item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: LVIS_SELECTED | LVIS_FOCUSED,
                ..LVITEMW::default()
            };
            unsafe {
                SendMessageW(
                    state.list,
                    LVM_SETITEMSTATE,
                    0,
                    &mut item as *mut LVITEMW as isize,
                );
                SetFocus(state.list);
            }
            true
        }
        VK_DOWN if !state.visible.is_empty() => {
            let mut item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: LVIS_SELECTED | LVIS_FOCUSED,
                ..LVITEMW::default()
            };
            unsafe {
                SendMessageW(
                    state.list,
                    LVM_SETITEMSTATE,
                    0,
                    &mut item as *mut LVITEMW as isize,
                );
                SetFocus(state.list);
            }
            true
        }
        _ => false,
    }
}

unsafe fn open_selected(window: HWND, state: &AppState, parent: bool) {
    let selected = unsafe {
        SendMessageW(
            state.list,
            LVM_GETNEXTITEM,
            usize::MAX,
            LVNI_SELECTED as isize,
        )
    };
    if selected < 0 {
        return;
    }
    let index = selected as usize;
    let Some(record) = state
        .visible
        .get(index)
        .and_then(|record| state.records.get(*record as usize))
    else {
        return;
    };
    let target = if parent {
        record.parent_path()
    } else {
        &record.path
    };
    let target = wide_null(target);
    let operation = wide_null("open");
    let result = unsafe {
        ShellExecuteW(
            window,
            operation.as_ptr(),
            target.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result <= 32 {
        unsafe {
            show_message(
                window,
                WINDOW_TITLE,
                "Windows could not open the selected result.",
            )
        };
    }
}

unsafe fn copy_selected_paths(window: HWND, state: &AppState) {
    let mut rows = Vec::new();
    let mut row = -1isize;
    loop {
        row = unsafe {
            SendMessageW(
                state.list,
                LVM_GETNEXTITEM,
                row as usize,
                LVNI_SELECTED as isize,
            )
        };
        if row < 0 {
            break;
        }
        if let Some(record) = state
            .visible
            .get(row as usize)
            .and_then(|index| state.records.get(*index as usize))
        {
            rows.push(record.path.as_str());
        }
    }
    if rows.is_empty() {
        return;
    }
    let text = wide_null(&rows.join("\r\n"));
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, text.len() * size_of::<u16>()) };
    if handle.is_null() {
        return;
    }
    let memory = unsafe { GlobalLock(handle) } as *mut u16;
    if memory.is_null() {
        unsafe { GlobalFree(handle) };
        return;
    }
    unsafe {
        copy_nonoverlapping(text.as_ptr(), memory, text.len());
        GlobalUnlock(handle);
        if OpenClipboard(window) == 0 {
            GlobalFree(handle);
            return;
        }
        EmptyClipboard();
        let transferred = !SetClipboardData(CF_UNICODETEXT, handle).is_null();
        CloseClipboard();
        if !transferred {
            GlobalFree(handle);
        }
    }
}

unsafe fn show_result_menu(window: HWND, state: &mut AppState) {
    if unsafe { SendMessageW(state.list, LVM_GETSELECTEDCOUNT, 0, 0) } == 0 {
        return;
    }
    let menu = unsafe { CreatePopupMenu() };
    append_menu(menu, MF_STRING, CMD_OPEN, "&Open");
    append_menu(menu, MF_STRING, CMD_OPEN_PATH, "Open &Path");
    append_menu(menu, MF_SEPARATOR, 0, "");
    append_menu(
        menu,
        MF_STRING,
        CMD_COPY_FULL_PATH,
        "Copy &Full Name to Clipboard",
    );
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) };
    let command = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            window,
            null(),
        )
    } as usize;
    unsafe { DestroyMenu(menu) };
    if command != 0 {
        unsafe { handle_command(window, state, command) };
    }
}

unsafe fn select_all(state: &AppState) {
    let mut item = LVITEMW {
        stateMask: LVIS_SELECTED,
        state: LVIS_SELECTED,
        ..LVITEMW::default()
    };
    unsafe {
        SendMessageW(
            state.list,
            LVM_SETITEMSTATE,
            usize::MAX,
            &mut item as *mut LVITEMW as isize,
        );
    }
}

unsafe fn update_selection_status(state: &AppState) {
    if !state.show_selected_item_in_statusbar {
        unsafe { set_object_status(state, state.visible.len()) };
        return;
    }
    let count = unsafe { SendMessageW(state.list, LVM_GETSELECTEDCOUNT, 0, 0) as usize };
    if count == 0 {
        unsafe { set_object_status(state, state.visible.len()) };
    } else if count == 1 {
        unsafe { set_status(state, "1 object selected") };
    } else {
        unsafe { set_status(state, &format!("{count} objects selected")) };
    }
}

unsafe fn set_object_status(state: &AppState, count: usize) {
    if count == 1 {
        unsafe { set_status(state, "1 object") };
    } else {
        unsafe { set_status(state, &format!("{count} objects")) };
    }
}

unsafe fn set_status(state: &AppState, text: &str) {
    let text = wide_null(text);
    unsafe { SendMessageW(state.status, SB_SETTEXTW, 0, text.as_ptr() as isize) };
}

fn column_text(record: &FileRecord, column: usize) -> String {
    match column {
        0 => record.file_name().to_owned(),
        1 => record.parent_path().to_owned(),
        2 => record.size.map(format_size).unwrap_or_default(),
        3 => record
            .date_modified
            .map(format_file_time)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn format_size(size: u64) -> String {
    let digits = size.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn format_file_time(value: u64) -> String {
    let file_time = FILETIME {
        dwLowDateTime: value as u32,
        dwHighDateTime: (value >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    if unsafe { FileTimeToSystemTime(&file_time, &mut utc) } == 0
        || unsafe { SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) } == 0
    {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    )
}

fn start_index_load(window: HWND, pipe_name: String, use_database: bool, force_reindex: bool) {
    let window_value = window as isize;
    thread::spawn(move || {
        let loaded = load_local_index(&pipe_name, use_database, force_reindex);
        let result = Box::into_raw(Box::new(LoadResult { loaded }));
        let posted =
            unsafe { PostMessageW(window_value as HWND, WM_INDEX_LOADED, 0, result as isize) };
        if posted == 0 {
            unsafe { drop(Box::from_raw(result)) };
        }
    });
}

fn load_local_index(
    pipe_name: &str,
    use_database: bool,
    force_reindex: bool,
) -> Result<LoadedIndex, String> {
    let database_path = database::default_path();
    let snapshot =
        database::load_local_snapshot(&database_path, pipe_name, use_database, force_reindex)?;
    let index = SharedIndex::restore(
        &snapshot.records,
        snapshot.volumes.iter().map(|volume| {
            (
                volume.volume_serial,
                volume.root.clone(),
                volume.root_file_reference,
            )
        }),
    )
    .map_err(str::to_owned)?;
    Ok(LoadedIndex {
        records: snapshot.records,
        index,
        checkpoints: snapshot.volumes,
    })
}

fn start_monitor(
    window: HWND,
    index: SharedIndex,
    checkpoints: Vec<VolumeCheckpoint>,
    pipe_name: String,
    use_database: bool,
    generation: u64,
) -> monitor::Monitor {
    let window_value = window as isize;
    monitor::start(
        index,
        checkpoints,
        pipe_name,
        database::default_path(),
        use_database,
        move |event| {
            let records = match event {
                MonitorEvent::Updated(records) => Ok(records),
                MonitorEvent::Error(error) => Err(error),
            };
            let result = Box::into_raw(Box::new(MonitorResult {
                generation,
                records,
            }));
            let posted =
                unsafe { PostMessageW(window_value as HWND, WM_INDEX_UPDATED, 0, result as isize) };
            if posted == 0 {
                unsafe { drop(Box::from_raw(result)) };
            }
        },
    )
}

unsafe fn insert_columns(list: HWND) {
    let columns = [
        ("Name", 260, LVCFMT_LEFT),
        ("Path", 360, LVCFMT_LEFT),
        ("Size", 100, LVCFMT_RIGHT),
        ("Date Modified", 140, LVCFMT_LEFT),
    ];
    for (index, (name, width, format)) in columns.into_iter().enumerate() {
        let mut name = wide_null(name);
        let mut column = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
            fmt: format,
            cx: width,
            pszText: name.as_mut_ptr(),
            ..LVCOLUMNW::default()
        };
        unsafe {
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                index,
                &mut column as *mut LVCOLUMNW as isize,
            );
        }
    }
}

fn create_child(
    parent: HWND,
    class_name: *const u16,
    text: &str,
    style: u32,
    id: usize,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
) -> Result<HWND, String> {
    let text = wide_null(text);
    create_child_raw(parent, class_name, text.as_ptr(), style, id, instance)
}

fn create_child_raw(
    parent: HWND,
    class_name: *const u16,
    text: *const u16,
    style: u32,
    id: usize,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
) -> Result<HWND, String> {
    let window = unsafe {
        CreateWindowExW(
            0,
            class_name,
            text,
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
    if window.is_null() {
        Err("create EveRusthing GUI control failed".into())
    } else {
        Ok(window)
    }
}

unsafe fn create_main_menu() -> *mut core::ffi::c_void {
    let menu = unsafe { CreateMenu() };
    let file = unsafe { CreatePopupMenu() };
    append_menu(
        file,
        MF_STRING,
        CMD_NEW_WINDOW,
        "&New Search Window\tCtrl+N",
    );
    append_menu(file, MF_SEPARATOR, 0, "");
    append_menu(file, MF_STRING, CMD_CLOSE, "&Close\tCtrl+W");
    append_menu(file, MF_STRING, CMD_EXIT, "E&xit");

    let edit = unsafe { CreatePopupMenu() };
    append_menu(
        edit,
        MF_STRING,
        CMD_COPY_FULL_PATH,
        "Copy &Full Name to Clipboard",
    );
    append_menu(edit, MF_SEPARATOR, 0, "");
    append_menu(edit, MF_STRING, CMD_SELECT_ALL, "Select &All\tCtrl+A");

    let view = unsafe { CreatePopupMenu() };
    append_menu(view, MF_STRING, CMD_STATUS_BAR, "&Status Bar");

    let search = unsafe { CreatePopupMenu() };
    append_menu(search, MF_STRING, CMD_MATCH_CASE, "Match &Case");
    append_menu(search, MF_STRING, CMD_MATCH_WHOLE_WORD, "Match &Whole Word");
    append_menu(search, MF_STRING, CMD_MATCH_PATH, "Match &Path");

    let tools = unsafe { CreatePopupMenu() };
    append_menu(tools, MF_STRING, CMD_OPTIONS, "&Options...");

    let bookmarks = unsafe { CreatePopupMenu() };
    append_menu(bookmarks, MF_STRING, CMD_BOOKMARKS, "&Add to Bookmarks...");
    append_menu(
        bookmarks,
        MF_STRING,
        CMD_BOOKMARKS,
        "&Organize Bookmarks...",
    );

    let help = unsafe { CreatePopupMenu() };
    append_menu(help, MF_STRING, CMD_HELP, "Everything &Help");
    append_menu(help, MF_SEPARATOR, 0, "");
    append_menu(help, MF_STRING, CMD_ABOUT, "&About EveRusthing...");

    append_menu(menu, MF_POPUP, file as usize, "&File");
    append_menu(menu, MF_POPUP, edit as usize, "&Edit");
    append_menu(menu, MF_POPUP, view as usize, "&View");
    append_menu(menu, MF_POPUP, search as usize, "&Search");
    append_menu(menu, MF_POPUP, bookmarks as usize, "&Bookmarks");
    append_menu(menu, MF_POPUP, tools as usize, "&Tools");
    append_menu(menu, MF_POPUP, help as usize, "&Help");
    menu
}

fn append_menu(menu: *mut core::ffi::c_void, flags: u32, id: usize, text: &str) {
    let text = wide_null(text);
    unsafe {
        AppendMenuW(
            menu,
            flags,
            id,
            if flags & MF_SEPARATOR != 0 {
                null()
            } else {
                text.as_ptr()
            },
        );
    }
}

unsafe fn sync_menu_checks(window: HWND, state: &AppState) {
    let menu = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetMenu(window) };
    check_menu(menu, CMD_STATUS_BAR, state.show_status_bar);
    check_menu(menu, CMD_MATCH_CASE, state.options.match_case);
    check_menu(menu, CMD_MATCH_WHOLE_WORD, state.options.match_whole_word);
    check_menu(menu, CMD_MATCH_PATH, state.options.match_path);
}

fn check_menu(menu: *mut core::ffi::c_void, command: usize, checked: bool) {
    unsafe {
        CheckMenuItem(
            menu,
            command as u32,
            MF_BYCOMMAND | if checked { MF_CHECKED } else { MF_UNCHECKED },
        );
    }
}

unsafe fn window_text(window: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(window) }.max(0) as usize;
    let mut text = vec![0u16; length + 1];
    let copied = unsafe { GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32) };
    String::from_utf16_lossy(&text[..copied.max(0) as usize])
}

unsafe fn copy_wide_to_buffer(text: &str, destination: *mut u16, capacity: i32) {
    if destination.is_null() || capacity <= 0 {
        return;
    }
    let text: Vec<u16> = text.encode_utf16().collect();
    let count = text.len().min(capacity as usize - 1);
    unsafe {
        copy_nonoverlapping(text.as_ptr(), destination, count);
        *destination.add(count) = 0;
    }
}

unsafe fn show_message(window: HWND, title: &str, text: &str) {
    let title = wide_null(title);
    let text = wide_null(text);
    unsafe {
        MessageBoxW(
            window,
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn settings_path() -> PathBuf {
    env::current_exe()
        .ok()
        .map(|path| path.with_file_name("EveRusthing.ini"))
        .unwrap_or_else(|| PathBuf::from("EveRusthing.ini"))
}

fn save_settings(window: HWND, state: &AppState) {
    let mut placement = windows_sys::Win32::UI::WindowsAndMessaging::WINDOWPLACEMENT {
        length: size_of::<windows_sys::Win32::UI::WindowsAndMessaging::WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    if unsafe { GetWindowPlacement(window, &mut placement) } == 0 {
        return;
    }
    let rectangle = placement.rcNormalPosition;
    let settings = GuiSettings {
        x: rectangle.left,
        y: rectangle.top,
        width: rectangle.right - rectangle.left,
        height: rectangle.bottom - rectangle.top,
        maximized: placement.showCmd == SW_SHOWMAXIMIZED as u32,
        show_status_bar: state.show_status_bar,
        show_selected_item_in_statusbar: state.show_selected_item_in_statusbar,
        search_as_you_type: state.search_as_you_type,
        options: state.options,
    };
    let _ = fs::write(&state.settings_path, settings.serialize());
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_settings_round_trip() {
        let settings = GuiSettings {
            x: 12,
            y: 34,
            width: 900,
            height: 600,
            maximized: true,
            show_status_bar: false,
            show_selected_item_in_statusbar: false,
            search_as_you_type: false,
            options: QueryOptions {
                match_case: true,
                match_path: true,
                match_whole_word: false,
            },
        };
        assert_eq!(GuiSettings::parse(&settings.serialize()), settings);
    }

    #[test]
    fn formats_grouped_byte_sizes() {
        assert_eq!(format_size(0), "0");
        assert_eq!(format_size(999), "999");
        assert_eq!(format_size(1_234_567), "1,234,567");
    }

    #[test]
    fn only_plain_filename_terms_reuse_previous_results() {
        assert!(is_simple_search("report"));
        assert!(!is_simple_search("two words"));
        assert!(!is_simple_search(r"src\main"));
        assert!(!is_simple_search("ext:rs"));
        assert!(!is_simple_search("*.rs"));
    }
}
