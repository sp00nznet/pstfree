//! A window for reading a PST or OST: folders on the left, messages top right, the
//! selected message underneath.
//!
//! Plain Win32 against the common controls that ship with Windows, so this is one
//! executable with nothing to install and no runtime behind it — the same deal as the
//! command line tool, which is the whole point of the project.
//!
//! The unsafe here is all FFI. Every pointer handed to Windows either points at a local
//! that outlives the call or at a boxed value whose ownership is spelled out where it is
//! created.

#![windows_subsystem = "windows"]

use pstfree::export::{self, Format};
use pstfree::ltp::{
    body_text, clean_subject, filetime, read_node_pc, NID_ROOT_FOLDER, PID_DELIVERY_TIME,
    PID_DISPLAY_NAME, PID_SENDER_NAME, PID_SUBJECT, PID_SUBMIT_TIME, PID_TRANSPORT_HEADERS,
};
use pstfree::ndb::{Block, Node, Pst};
use std::collections::BTreeMap;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateFontIndirectW, CreateSolidBrush, HFONT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_F5};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_TREE: isize = 1;
const ID_LIST: isize = 2;
const ID_TEXT: isize = 3;
const ID_SEARCH: isize = 4;
const ID_OPEN: usize = 100;
const ID_EXPORT_EML: usize = 101;
const ID_EXPORT_MBOX: usize = 102;
const ID_EXPORT_MSG: usize = 103;
const ID_REBUILD: usize = 104;
const ID_REPORT: usize = 105;
const ID_QUIT: usize = 106;
const ID_FIND: usize = 107;
const ID_EXPORT_FOLDER: usize = 108;
const ID_EXPORT_MESSAGE: usize = 109;

/// The icons, embedded.
///
/// Rust cannot put a Windows resource in an executable without a build script, and this
/// project deliberately has none — the README's promise is one dependency and one command.
/// So the `.ico` files are compiled in as bytes and turned into icons at run time, which
/// needs nothing but the `CreateIconFromResourceEx` that `LoadIcon` calls anyway.
///
/// All of them come out of asset-forge's `pipelines/ui_icons.py`, which draws them from
/// geometry rather than scaling a large bitmap down — the one place a generated image is
/// always wrong is the 16-pixel icon. Same generator as opennote's, one accent hue apart,
/// so the family of free tools looks like a family.
///
// ponytail: this gives the title bar, the taskbar and Alt-Tab. The icon Explorer shows
// for the .exe file itself is a real RT_GROUP_ICON resource, which needs the build script
// this is avoiding. Add one if the download page ever looks wrong enough to matter.
const APP_ICON: &[u8] = include_bytes!("../../res/pstfree.ico");
const GLYPHS: [&[u8]; 5] = [
    include_bytes!("../../res/open.ico"),
    include_bytes!("../../res/save.ico"),
    include_bytes!("../../res/new.ico"),
    include_bytes!("../../res/warning.ico"),
    include_bytes!("../../res/search.ico"),
];

/// The version, so a downloaded .exe with no installer behind it can still say what it
/// is. It goes in the title bar, which is the only place a window can put it and be sure
/// somebody reading a bug report can find it.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A long job posts these back: progress as a boxed `String`, the end as a boxed `Done`,
/// and the handler takes ownership of either. Posting is the one thing Windows lets another thread do to a window, so the job
/// runs off the message loop and the window keeps painting while it does.
const WM_JOB_PROGRESS: u32 = WM_APP + 1;
const WM_JOB_DONE: u32 = WM_APP + 2;

/// A window handle to post to from the worker. `PostMessageW` is documented as safe to
/// call from any thread; nothing else here is done with it.
#[derive(Clone, Copy)]
struct Poster(HWND);
unsafe impl Send for Poster {}

/// What a finished job hands back to the window.
enum Done {
    /// Something to say, in the status bar and a message box.
    Text(String),
    /// A search: the needle it was run for, and what it found.
    Found(String, Vec<pstfree::ltp::Hit>),
}

impl Poster {
    /// Hand the window a string and give up ownership of it; the handler takes it back.
    /// Taken by value so the whole handle moves into a worker, rather than the raw
    /// pointer inside it, which is the part that is not `Send`.
    fn say(self, text: String) {
        unsafe {
            PostMessageW(
                self.0,
                WM_JOB_PROGRESS,
                0,
                Box::into_raw(Box::new(text)) as LPARAM,
            )
        };
    }

    fn done(self, done: Done) {
        unsafe {
            PostMessageW(
                self.0,
                WM_JOB_DONE,
                0,
                Box::into_raw(Box::new(done)) as LPARAM,
            )
        };
    }
}

/// Everything the window needs to answer a message. Boxed once and hung off the window.
struct App {
    pst: Option<Pst>,
    path: String,
    nodes: Vec<Node>,
    /// Folder id -> its messages, newest first, as (node, subject, sender, date).
    by_folder: BTreeMap<u32, Vec<(Node, String, String, String)>>,
    /// What the list is currently showing.
    shown: Vec<Node>,
    /// Folder id -> display name. The tree, a search result and an export of one folder
    /// all need these, and they have to be the same names or the folder called Inbox on
    /// screen is not the one written to disk.
    names: BTreeMap<u32, String>,
    /// The folder the tree has selected, so "export this folder" knows which one.
    /// Zero while a search result is showing, which belongs to no folder.
    folder: u32,
    /// The display's pixel density, so the pane limits can be stated in the pixels
    /// somebody actually has rather than in the ones a 2005 monitor had.
    dpi: i32,
    tree: HWND,
    list: HWND,
    text: HWND,
    search: HWND,
    toolbar: HWND,
    status: HWND,
    /// What was wrong with the file, kept so the report can be asked for again rather
    /// than scrolling past once in the status bar.
    problems: Vec<String>,
    /// Whether the index had to be swept, which the report says out loud.
    salvaged: bool,
    /// A job is running on another thread. The menu items that would start a second one
    /// are refused rather than greyed out, so the refusal can say why.
    busy: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(b: &[u16]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..n])
}

fn main() {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide("pstfree_window");

        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_TREEVIEW_CLASSES | ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES,
        };
        InitCommonControlsEx(&icc);

        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: LoadIconW(std::ptr::null_mut(), IDI_APPLICATION),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            // WS_CLIPCHILDREN, with WS_CLIPSIBLINGS on every child: the search box sits
            // inside the toolbar's rectangle, and without these two the toolbar paints
            // over it. The box is still there, still focusable, still holds what is typed
            // into it — it simply draws nothing, which is a maddening thing to debug.
            hbrBackground: CreateSolidBrush(0x00F0F0F0 as COLORREF),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);

        // A window is created in real pixels, and now that the program is DPI-aware
        // Windows no longer stretches what it asks for. 1100x720 on a display running at
        // 200% is half the window it used to be, with text the full size — so the size
        // has to be scaled by hand, which is the bill that comes with being sharp.
        let dpi = screen_dpi();

        let title = wide("pstfree");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1100 * dpi / 96,
            720 * dpi / 96,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );

        // A file named on the command line opens straight away, so the window can be
        // dropped on a .pst as well as launched on its own.
        if let Some(arg) = std::env::args().nth(1) {
            let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
            if !app.is_null() {
                open_file(hwnd, &mut *app, &arg);
            }
        }

        // The menu has advertised Ctrl+O since the window was written and nothing was
        // listening for it. A three-entry table is the whole of the fix.
        let accel = [
            ACCEL {
                fVirt: (FCONTROL | FVIRTKEY),
                key: b'O' as u16,
                cmd: ID_OPEN as u16,
            },
            ACCEL {
                fVirt: (FCONTROL | FVIRTKEY),
                key: b'F' as u16,
                cmd: ID_FIND as u16,
            },
            ACCEL {
                fVirt: FVIRTKEY,
                key: VK_F5,
                cmd: ID_REPORT as u16,
            },
        ];
        let table = CreateAcceleratorTableW(accel.as_ptr(), accel.len() as i32);

        ShowWindow(hwnd, SW_SHOW);
        let mut msg = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if TranslateAcceleratorW(hwnd, table, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            create_children(hwnd);
            0
        }
        WM_SIZE => {
            let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
            if !app.is_null() {
                layout(hwnd, &*app);
            }
            0
        }
        WM_COMMAND => {
            let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
            if !app.is_null() {
                command(hwnd, &mut *app, wp & 0xFFFF);
            }
            0
        }
        WM_NOTIFY => {
            let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
            if !app.is_null() {
                notify(&mut *app, lp);
            }
            0
        }
        // Both carry something the worker boxed and gave up ownership of.
        WM_JOB_PROGRESS => {
            let text = *Box::from_raw(lp as *mut String);
            set_status(hwnd, &text);
            0
        }
        WM_JOB_DONE => {
            let done = *Box::from_raw(lp as *mut Done);
            let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
            if app.is_null() {
                return 0;
            }
            (*app).busy = false;
            match done {
                Done::Text(text) => {
                    set_status(hwnd, &text.replace('\n', " "));
                    message_box(hwnd, &text, "pstfree", MB_ICONINFORMATION);
                }
                Done::Found(needle, hits) => show_hits(hwnd, &mut *app, &needle, &hits),
            }
            0
        }
        WM_DESTROY => {
            let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
            if !app.is_null() {
                drop(Box::from_raw(app));
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// One entry out of an embedded `.ico`, as an icon at the size asked for.
///
/// An ICO is a six-byte header, a sixteen-byte directory entry each, and then the images,
/// so picking the right one is a dozen lines rather than a dependency. The entry nearest
/// the wanted size wins, and a smaller one loses to a larger one twice over: scaling a
/// 16-pixel drawing up to 32 is visibly worse than scaling a 32 down.
unsafe fn icon(ico: &[u8], want: i32) -> HICON {
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let u32at =
        |o: usize| u32::from_le_bytes([ico[o], ico[o + 1], ico[o + 2], ico[o + 3]]) as usize;

    let mut best: Option<(i32, usize, usize)> = None;
    for i in 0..count {
        let e = 6 + i * 16;
        if e + 16 > ico.len() {
            break;
        }
        // A zero in the width byte means 256, which does not fit in a byte.
        let w = if ico[e] == 0 { 256 } else { ico[e] as i32 };
        let (len, off) = (u32at(e + 8), u32at(e + 12));
        if off + len > ico.len() {
            continue;
        }
        let cost = (w - want).abs() * if w < want { 2 } else { 1 };
        if best.is_none_or(|(b, _, _)| cost < b) {
            best = Some((cost, off, len));
        }
    }

    match best {
        Some((_, off, len)) => CreateIconFromResourceEx(
            ico[off..].as_ptr(),
            len as u32,
            1,
            0x0003_0000, // the icon-resource version Windows has wanted since NT
            want,
            want,
            LR_DEFAULTCOLOR,
        ),
        None => std::ptr::null_mut(),
    }
}

/// Dots per inch for the display this program is running on.
///
/// Read off a screen DC rather than through the DPI API, which lives behind a
/// windows-sys feature this crate would otherwise not need. The manifest declares system
/// DPI awareness, so this is the one number that matters and it does not change while the
/// program is running.
unsafe fn screen_dpi() -> i32 {
    use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC, LOGPIXELSX};
    let dc = GetDC(std::ptr::null_mut());
    let d = GetDeviceCaps(dc, LOGPIXELSX as i32);
    ReleaseDC(std::ptr::null_mut(), dc);
    if d > 0 {
        d
    } else {
        96
    }
}

/// The font the rest of Windows is using.
///
/// `DEFAULT_GUI_FONT` — the obvious stock object, and what this window used to ask for —
/// is still the bitmap font from 1995. It is not what any other window on the machine is
/// drawn in, it does not follow the user's text size, and it is the single loudest reason
/// a plain Win32 program looks abandoned. The shell keeps the real one in the
/// non-client metrics, where it has been all along.
unsafe fn ui_font() -> HFONT {
    let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
    ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
    if SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        &mut ncm as *mut _ as *mut c_void,
        0,
    ) != 0
    {
        let f = CreateFontIndirectW(&ncm.lfMessageFont);
        if !f.is_null() {
            return f;
        }
    }
    windows_sys::Win32::Graphics::Gdi::GetStockObject(
        windows_sys::Win32::Graphics::Gdi::DEFAULT_GUI_FONT,
    ) as HFONT
}

/// Return and Escape in the search box, which an EDIT control otherwise swallows with a
/// beep. Typing a search and pressing Enter is the only way anybody expects a search box
/// to work, and there is no dialog manager here to arrange it.
unsafe extern "system" fn search_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_CHAR && (wp == 13 || wp == 27) {
        let parent = GetParent(h);
        if wp == 27 {
            let empty = wide("");
            SetWindowTextW(h, empty.as_ptr());
        }
        SendMessageW(parent, WM_COMMAND, ID_FIND, 0);
        return 0;
    }
    DefSubclassProc(h, msg, wp, lp)
}

unsafe fn create_children(hwnd: HWND) {
    let instance = GetModuleHandleW(std::ptr::null());
    let font = ui_font();

    let mk = |class: &str, style: u32, id: isize| -> HWND {
        let c = wide(class);
        let h = CreateWindowExW(
            0,
            c.as_ptr(),
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | style,
            0,
            0,
            0,
            0,
            hwnd,
            id as *mut c_void,
            instance,
            std::ptr::null(),
        );
        SendMessageW(h, WM_SETFONT, font as WPARAM, 1);
        h
    };

    let tree = mk(
        "SysTreeView32",
        WS_BORDER | TVS_HASBUTTONS | TVS_HASLINES | TVS_LINESATROOT,
        ID_TREE,
    );
    let list = mk(
        "SysListView32",
        WS_BORDER | LVS_REPORT | LVS_SINGLESEL,
        ID_LIST,
    );
    let text = mk(
        "EDIT",
        WS_BORDER | WS_VSCROLL | WS_HSCROLL | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32,
        ID_TEXT,
    );
    let search = mk("EDIT", WS_BORDER | (ES_AUTOHSCROLL) as u32, ID_SEARCH);
    let status = mk("msctls_statusbar32", SBARS_SIZEGRIP, 0);

    // A toolbar, drawn from the same geometry as the application icon. The four things
    // this program does, in the order somebody does them, and the search box beside them.
    let toolbar = CreateWindowExW(
        0,
        wide("ToolbarWindow32").as_ptr(),
        std::ptr::null(),
        WS_CHILD
            | WS_VISIBLE
            | WS_CLIPSIBLINGS
            | TBSTYLE_FLAT
            | TBSTYLE_LIST
            | CCS_NODIVIDER as u32,
        0,
        0,
        0,
        0,
        hwnd,
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    SendMessageW(toolbar, WM_SETFONT, font as WPARAM, 1);
    SendMessageW(toolbar, TB_BUTTONSTRUCTSIZE, size_of::<TBBUTTON>(), 0);

    // Scaled off the system's small-icon metric rather than fixed at 24, so the buttons
    // are the size of everything else on a display running at 150%.
    let gsz = (GetSystemMetrics(SM_CXSMICON) * 3 / 2).max(16);
    let images = ImageList_Create(gsz, gsz, ILC_COLOR32 | ILC_MASK, GLYPHS.len() as i32, 0);
    for g in GLYPHS {
        ImageList_ReplaceIcon(images, -1, icon(g, gsz));
    }
    SendMessageW(toolbar, TB_SETIMAGELIST, 0, images as LPARAM);

    let labels: Vec<Vec<u16>> = ["Open", "Export", "Repair", "Problems", "Find"]
        .iter()
        .map(|s| wide(s))
        .collect();
    let mut buttons: Vec<TBBUTTON> = Vec::new();
    for (i, id) in [ID_OPEN, ID_EXPORT_EML, ID_REBUILD, ID_REPORT, ID_FIND]
        .into_iter()
        .enumerate()
    {
        // A separator before Find, because searching is not one of the four file actions
        // and a toolbar that does not say so reads as five equal things.
        if id == ID_FIND {
            let mut sep: TBBUTTON = std::mem::zeroed();
            sep.fsStyle = BTNS_SEP as u8;
            buttons.push(sep);
        }
        let mut b: TBBUTTON = std::mem::zeroed();
        b.iBitmap = i as i32;
        b.idCommand = id as i32;
        b.fsState = TBSTATE_ENABLED as u8;
        b.fsStyle = BTNS_AUTOSIZE as u8;
        b.iString = labels[i].as_ptr() as isize;
        buttons.push(b);
    }
    SendMessageW(
        toolbar,
        TB_ADDBUTTONSW,
        buttons.len(),
        buttons.as_ptr() as LPARAM,
    );
    SendMessageW(toolbar, TB_AUTOSIZE, 0, 0);

    // The search box is a sibling of the toolbar and was created before it, which puts it
    // *under* the toolbar in z-order — so it sits in the toolbar's rectangle, reports
    // itself visible, holds whatever is typed into it, and paints nothing at all. Lifted
    // to the top here rather than by reordering the two, because the order they are
    // created in reads correctly and this is the thing that is actually being said.
    SetWindowPos(
        search,
        HWND_TOP,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );

    // The themed library draws a list and a tree the way Explorer does — hot tracking, a
    // selection that is not a blue slab, and no flicker on a list being refilled. It is
    // opt-in per control, which is why a manifest alone does not get it.
    SetWindowTheme(list, wide("Explorer").as_ptr(), std::ptr::null());
    SetWindowTheme(tree, wide("Explorer").as_ptr(), std::ptr::null());

    // The search box explains itself rather than needing a label beside it, and Return
    // runs the search — see `search_proc`.
    let cue = wide("Search subject, sender or body");
    SendMessageW(search, EM_SETCUEBANNER, 1, cue.as_ptr() as LPARAM);
    SetWindowSubclass(search, Some(search_proc), 0, 0);

    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        0,
        (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as LPARAM,
    );
    for (i, (title, width)) in [
        ("Date", 130),
        ("From", 180),
        ("Subject", 400),
        ("Folder", 150),
    ]
    .iter()
    .enumerate()
    {
        let t = wide(title);
        let col = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH,
            fmt: 0,
            cx: *width,
            pszText: t.as_ptr() as *mut u16,
            cchTextMax: 0,
            iSubItem: 0,
            iImage: 0,
            iOrder: 0,
            cxMin: 0,
            cxDefault: 0,
            cxIdeal: 0,
        };
        SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as LPARAM);
    }

    let app = Box::new(App {
        pst: None,
        path: String::new(),
        nodes: Vec::new(),
        by_folder: BTreeMap::new(),
        shown: Vec::new(),
        names: BTreeMap::new(),
        folder: 0,
        dpi: screen_dpi(),
        tree,
        list,
        text,
        search,
        toolbar,
        status,
        problems: Vec::new(),
        salvaged: false,
        busy: false,
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);

    let menu = CreateMenu();
    let file = CreateMenu();
    let items = [
        (ID_OPEN, "&Open PST or OST...\tCtrl+O"),
        (0, ""),
        (ID_EXPORT_EML, "Export everything as &.eml..."),
        (ID_EXPORT_MBOX, "Export everything as m&box..."),
        (ID_EXPORT_MSG, "Export everything as .m&sg..."),
        (0, ""),
        (ID_EXPORT_FOLDER, "Export this &folder as .eml..."),
        (ID_EXPORT_MESSAGE, "Export this &message as .eml..."),
        (0, ""),
        (ID_FIND, "F&ind in this file\tCtrl+F"),
        (ID_REBUILD, "&Repair to a new .pst..."),
        (ID_REPORT, "&What is wrong with this file"),
        (0, ""),
        (ID_QUIT, "E&xit"),
    ];
    for (id, label) in items {
        if id == 0 {
            AppendMenuW(file, MF_SEPARATOR, 0, std::ptr::null());
        } else {
            let l = wide(label);
            AppendMenuW(file, MF_STRING, id, l.as_ptr());
        }
    }
    let f = wide("&File");
    AppendMenuW(menu, MF_POPUP, file as usize, f.as_ptr());
    SetMenu(hwnd, menu);

    // Big for Alt-Tab, small for the title bar and the taskbar. Both are the same
    // drawing, but the small entry is not the large one scaled down — see ui_icons.py.
    SendMessageW(
        hwnd,
        WM_SETICON,
        ICON_BIG as WPARAM,
        icon(APP_ICON, GetSystemMetrics(SM_CXICON)) as LPARAM,
    );
    SendMessageW(
        hwnd,
        WM_SETICON,
        ICON_SMALL as WPARAM,
        icon(APP_ICON, GetSystemMetrics(SM_CXSMICON)) as LPARAM,
    );

    set_status(
        hwnd,
        "Open a .pst or .ost file to begin. No password is ever needed.",
    );
    let title = wide(&format!("pstfree {VERSION}"));
    SetWindowTextW(hwnd, title.as_ptr());
}

/// Toolbar and search box across the top, folders down the left, and the rest split
/// between the message list and the message itself.
unsafe fn layout(hwnd: HWND, app: &App) {
    let mut rc = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);

    SendMessageW(app.status, WM_SIZE, 0, 0);
    let mut sr = std::mem::zeroed();
    GetWindowRect(app.status, &mut sr);
    let sh = sr.bottom - sr.top;

    // The toolbar sizes itself to its buttons and its font, so it is asked rather than
    // told — that is what keeps the row the right height at 150% as well as at 100%.
    SendMessageW(app.toolbar, TB_AUTOSIZE, 0, 0);
    let mut tr = std::mem::zeroed();
    GetWindowRect(app.toolbar, &mut tr);
    let th = tr.bottom - tr.top;
    let pad = th / 6;

    MoveWindow(app.toolbar, 0, 0, w, th, 1);
    // The box takes the right third of the row, with a floor so it is still usable in a
    // narrow window and a ceiling so it is not a hundred characters wide in a wide one.
    //
    // Both of those, and the folder pane's cap below, are in real pixels — so they are
    // scaled, because now that the program is DPI-aware a "340 pixel" folder pane on a
    // display running at 200% is half the width it was meant to be and every folder name
    // in it is cut off. A limit expressed in pixels has to say which pixels.
    let dpi = app.dpi;
    let sw = (w / 3).clamp((160 * dpi / 96).min(w), 420 * dpi / 96);
    MoveWindow(app.search, w - sw - pad, pad, sw, th - pad * 2, 1);

    let body = h - sh - th;
    let tree_w = (w / 3).min(340 * dpi / 96);
    let list_h = body / 2;
    MoveWindow(app.tree, 0, th, tree_w, body, 1);
    MoveWindow(app.list, tree_w, th, w - tree_w, list_h, 1);
    MoveWindow(app.text, tree_w, th + list_h, w - tree_w, body - list_h, 1);
}

unsafe fn set_status(hwnd: HWND, s: &str) {
    let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if app.is_null() {
        return;
    }
    let t = wide(s);
    SendMessageW((*app).status, SB_SETTEXTW, 0, t.as_ptr() as LPARAM);
}

unsafe fn message_box(hwnd: HWND, text: &str, caption: &str, style: u32) -> i32 {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), style)
}

unsafe fn command(hwnd: HWND, app: &mut App, id: usize) {
    match id {
        ID_OPEN => {
            if let Some(path) = pick_file(hwnd) {
                open_file(hwnd, app, &path);
            }
        }
        ID_EXPORT_EML => do_export(hwnd, app, Format::Eml, Scope::Everything),
        ID_EXPORT_MBOX => do_export(hwnd, app, Format::Mbox, Scope::Everything),
        ID_EXPORT_MSG => do_export(hwnd, app, Format::Msg, Scope::Everything),
        ID_EXPORT_FOLDER => do_export(hwnd, app, Format::Eml, Scope::Folder),
        ID_EXPORT_MESSAGE => do_export(hwnd, app, Format::Eml, Scope::Message),
        ID_FIND => do_find(hwnd, app),
        ID_REBUILD => do_rebuild(hwnd, app),
        ID_REPORT => do_report(hwnd, app),
        ID_QUIT => {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        _ => {}
    }
}

/// The standard open dialog, through the old flat API so no COM is involved.
unsafe fn pick_file(hwnd: HWND) -> Option<String> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_FILEMUSTEXIST, OPENFILENAMEW,
    };

    let mut buf = [0u16; 1024];
    let filter: Vec<u16> = "Outlook data files\0*.pst;*.ost\0All files\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title = wide("Open a PST or OST file");

    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = hwnd;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST;

    (GetOpenFileNameW(&mut ofn) != 0).then(|| from_wide(&buf))
}

/// Where to write a repair. The same flat API as the open dialog, one flag apart.
unsafe fn pick_save(hwnd: HWND) -> Option<String> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetSaveFileNameW, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    let mut buf = [0u16; 1024];
    let filter: Vec<u16> = "Outlook data file\0*.pst\0All files\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title = wide("Write the repaired copy as");
    let ext = wide("pst");

    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = hwnd;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.lpstrDefExt = ext.as_ptr();
    ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST;

    (GetSaveFileNameW(&mut ofn) != 0).then(|| from_wide(&buf))
}

/// Pick a directory, using the folder-browser rather than the file dialog.
unsafe fn pick_folder(hwnd: HWND) -> Option<String> {
    use windows_sys::Win32::UI::Shell::{
        SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
        BROWSEINFOW,
    };

    let title = wide("Choose an empty folder to export into");
    let mut bi: BROWSEINFOW = std::mem::zeroed();
    bi.hwndOwner = hwnd;
    bi.lpszTitle = title.as_ptr();
    bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;

    let idl = SHBrowseForFolderW(&bi);
    if idl.is_null() {
        return None;
    }
    let mut buf = [0u16; 1024];
    let ok = SHGetPathFromIDListW(idl, buf.as_mut_ptr()) != 0;
    ok.then(|| from_wide(&buf))
}

/// Open a file and get an index out of it, however that has to be done.
///
/// A file whose index is gone is the one someone most wants opened, so it is rebuilt by
/// sweeping and carving rather than refused. Shared with the worker thread, which opens
/// its own handle and has to arrive at exactly the same index the window is showing.
fn load(path: &str) -> Result<(Pst, Vec<Node>, bool), String> {
    let mut pst = Pst::open(path)?;
    let mut nodes = pst.nodes();
    let salvaged = nodes.is_empty() || pst.blocks().is_empty();
    if salvaged {
        let carved = pst.carve();
        pst.adopt(&carved);
        let r = pst.scan();
        if nodes.is_empty() {
            nodes = r.nodes;
        }
    }
    Ok((pst, nodes, salvaged))
}

/// The blocks a rebuild should copy: whatever the index in use names.
fn index_blocks(pst: &mut Pst) -> Vec<Block> {
    let blocks = pst.blocks();
    if blocks.is_empty() {
        pst.carve()
    } else {
        blocks
    }
}

/// A display name for every folder. Both the tree and an export need these, and they
/// have to agree, or the folder called Inbox on screen is not the one on disk.
fn folder_names(pst: &mut Pst, nodes: &[Node]) -> BTreeMap<u32, String> {
    let mut names = BTreeMap::new();
    for n in nodes.iter().filter(|n| n.nid_type() == 0x02) {
        let name = read_node_pc(pst, n)
            .ok()
            .and_then(|pc| pc.str(PID_DISPLAY_NAME).map(str::to_string))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                if n.nid == NID_ROOT_FOLDER {
                    "(root)".into()
                } else {
                    "(unnamed)".into()
                }
            });
        names.insert(n.nid, name);
    }
    names
}

unsafe fn open_file(hwnd: HWND, app: &mut App, path: &str) {
    set_status(hwnd, &format!("Reading {path}..."));
    let (mut pst, nodes, salvaged) = match load(path) {
        Ok(t) => t,
        Err(e) => {
            message_box(hwnd, &e, "pstfree", MB_ICONERROR);
            set_status(hwnd, "Nothing open.");
            return;
        }
    };

    app.problems = pst.warnings.clone();
    app.salvaged = salvaged;

    // The root folder is the one place the file's own name reads better than the file's
    // idea of the name, which is usually blank.
    let mut names = folder_names(&mut pst, &nodes);
    if let Some(root) = names.get_mut(&NID_ROOT_FOLDER) {
        if root == "(root)" {
            *root = path
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or("(root)")
                .to_string();
        }
    }

    let mut by_folder: BTreeMap<u32, Vec<(Node, String, String, String)>> = BTreeMap::new();
    let mut total = 0;
    for n in nodes.iter().filter(|n| n.nid_type() == 0x04) {
        let Ok(pc) = read_node_pc(&mut pst, n) else {
            continue;
        };
        let when = pc
            .time(PID_DELIVERY_TIME)
            .or(pc.time(PID_SUBMIT_TIME))
            .unwrap_or(0);
        by_folder.entry(n.nid_parent).or_default().push((
            *n,
            clean_subject(pc.str(PID_SUBJECT).unwrap_or("(no subject)")).to_string(),
            pc.str(PID_SENDER_NAME).unwrap_or("").to_string(),
            filetime(when).trim().to_string(),
        ));
        total += 1;
    }
    for v in by_folder.values_mut() {
        v.sort_by(|a, b| b.3.cmp(&a.3));
    }

    let problems = pst.problem_count();
    app.pst = Some(pst);
    app.path = path.to_string();
    app.nodes = nodes;
    app.by_folder = by_folder;
    app.names = names;

    // Cleared before the tree is filled, not after: filling it selects a folder and
    // populates the list, and clearing afterwards would wipe exactly that.
    SendMessageW(app.list, LVM_DELETEALLITEMS, 0, 0);
    let t = wide("");
    SendMessageW(app.text, WM_SETTEXT, 0, t.as_ptr() as LPARAM);
    fill_tree(app);

    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    set_status(
        hwnd,
        &format!(
            "{file} — {} folders, {total} messages{}{}",
            app.names.len(),
            if salvaged {
                ", index rebuilt by sweeping the file"
            } else {
                ""
            },
            if problems > 0 {
                format!(", {problems} problem(s) found")
            } else {
                String::new()
            }
        ),
    );

    let title = wide(&format!("pstfree {VERSION} — {file}"));
    SetWindowTextW(hwnd, title.as_ptr());
}

/// Fill the folder tree, following the node parent pointers.
unsafe fn fill_tree(app: &mut App) {
    // Cloned rather than borrowed: the loop below writes to `app` while reading these,
    // and cloned rather than taken, because `show_folder` at the end of this function and
    // every later lookup — the Folder column, an export of one folder — need them still
    // to be there. A few hundred folder names copied once per file opened.
    let names = app.names.clone();
    SendMessageW(app.tree, TVM_DELETEITEM, 0, TVI_ROOT as LPARAM);

    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for n in app.nodes.iter().filter(|n| n.nid_type() == 0x02) {
        if n.nid != n.nid_parent {
            children.entry(n.nid_parent).or_default().push(n.nid);
        }
    }

    // Depth-first with an explicit stack, and a seen set, because a damaged file can
    // present a parent chain that loops.
    let mut seen = std::collections::HashSet::new();
    let mut inserted: Vec<(*mut c_void, u32)> = Vec::new();
    let mut stack = vec![(NID_ROOT_FOLDER, TVI_ROOT)];
    while let Some((nid, parent_item)) = stack.pop() {
        if !seen.insert(nid) {
            continue;
        }
        let Some(name) = names.get(&nid) else {
            continue;
        };
        let count = app.by_folder.get(&nid).map_or(0, Vec::len);
        let label = if count > 0 {
            format!("{name}  ({count})")
        } else {
            name.clone()
        };
        let mut text = wide(&label);

        let mut item: TVINSERTSTRUCTW = std::mem::zeroed();
        item.hParent = parent_item;
        item.hInsertAfter = TVI_LAST;
        item.Anonymous.item.mask = TVIF_TEXT | TVIF_PARAM;
        item.Anonymous.item.pszText = text.as_mut_ptr();
        item.Anonymous.item.lParam = nid as LPARAM;
        let h =
            SendMessageW(app.tree, TVM_INSERTITEMW, 0, &item as *const _ as LPARAM) as *mut c_void;

        inserted.push((h, nid));
        for &c in children.get(&nid).map(Vec::as_slice).unwrap_or(&[]) {
            stack.push((c, h as _));
        }
    }

    // Expanding takes an item, not the tree - passing the tree's root expands nothing,
    // which leaves every folder hidden behind a single collapsed line.
    for (h, _) in &inserted {
        SendMessageW(app.tree, TVM_EXPAND, TVE_EXPAND as WPARAM, *h as LPARAM);
    }

    // Open on the fullest folder rather than on nothing, so the window has something in
    // it the moment a file is loaded.
    if let Some((h, nid)) = inserted
        .iter()
        .max_by_key(|(_, nid)| app.by_folder.get(nid).map_or(0, Vec::len))
        .filter(|(_, nid)| app.by_folder.contains_key(nid))
        .copied()
    {
        SendMessageW(app.tree, TVM_SELECTITEM, TVGN_CARET as WPARAM, h as LPARAM);
        // Filled directly rather than left to the selection notification: this runs while
        // the file is being opened, which is before the message loop is pumping, so the
        // notification is not a thing that can be relied on to have arrived.
        show_folder(app, nid);

        // ...and open on its newest message, for the same reason.
        if !app.shown.is_empty() {
            let mut sel: LVITEMW = std::mem::zeroed();
            sel.mask = LVIF_STATE;
            sel.state = LVIS_SELECTED | LVIS_FOCUSED;
            sel.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
            SendMessageW(app.list, LVM_SETITEMSTATE, 0, &sel as *const _ as LPARAM);
            show_message(app, 0);
        }
    }
}

unsafe fn notify(app: &mut App, lp: LPARAM) {
    let nmhdr = &*(lp as *const NMHDR);
    match (nmhdr.idFrom as isize, nmhdr.code) {
        (ID_TREE, TVN_SELCHANGEDW) => {
            let nm = &*(lp as *const NMTREEVIEWW);
            show_folder(app, nm.itemNew.lParam as u32);
        }
        (ID_LIST, LVN_ITEMCHANGED) => {
            let nm = &*(lp as *const NMLISTVIEW);
            if nm.uNewState & LVIS_SELECTED != 0 && nm.iItem >= 0 {
                show_message(app, nm.iItem as usize);
            }
        }
        _ => {}
    }
}

/// One row in the message list. The folder column is filled in either view: in a folder
/// it is the folder you are looking at, and in a search result it is the answer to the
/// only question a hit raises.
unsafe fn add_row(list: HWND, row: usize, date: &str, from: &str, subject: &str, folder: &str) {
    let mut d = wide(date);
    let mut item: LVITEMW = std::mem::zeroed();
    item.mask = LVIF_TEXT;
    item.iItem = row as i32;
    item.pszText = d.as_mut_ptr();
    SendMessageW(list, LVM_INSERTITEMW, 0, &item as *const _ as LPARAM);

    for (col, s) in [(1, from), (2, subject), (3, folder)] {
        let mut t = wide(s);
        let mut sub: LVITEMW = std::mem::zeroed();
        sub.mask = LVIF_TEXT;
        sub.iItem = row as i32;
        sub.iSubItem = col;
        sub.pszText = t.as_mut_ptr();
        SendMessageW(list, LVM_SETITEMW, 0, &sub as *const _ as LPARAM);
    }
}

unsafe fn show_folder(app: &mut App, nid: u32) {
    SendMessageW(app.list, LVM_DELETEALLITEMS, 0, 0);
    app.shown.clear();
    app.folder = nid;
    let name = app.names.get(&nid).cloned().unwrap_or_default();
    let Some(msgs) = app.by_folder.get(&nid) else {
        return;
    };

    for (i, (node, subject, from, date)) in msgs.iter().enumerate() {
        add_row(app.list, i, date, from, subject, &name);
        app.shown.push(*node);
    }
}

unsafe fn show_message(app: &mut App, row: usize) {
    let Some(node) = app.shown.get(row).copied() else {
        return;
    };
    let Some(pst) = app.pst.as_mut() else { return };

    let body = match read_node_pc(pst, &node) {
        Err(e) => format!("This message could not be read.\r\n\r\n{e}"),
        Ok(pc) => {
            let mut s = String::new();
            // The original headers where the message kept them, since they are the most
            // faithful thing in the file; otherwise the few properties worth showing.
            match pc.str(PID_TRANSPORT_HEADERS) {
                Some(h) => s.push_str(&h.replace("\r\n", "\n").replace('\n', "\r\n")),
                None => {
                    for (label, v) in [
                        ("From", pc.str(PID_SENDER_NAME).unwrap_or("").to_string()),
                        (
                            "Subject",
                            clean_subject(pc.str(PID_SUBJECT).unwrap_or("")).to_string(),
                        ),
                        (
                            "Date",
                            filetime(
                                pc.time(PID_DELIVERY_TIME)
                                    .or(pc.time(PID_SUBMIT_TIME))
                                    .unwrap_or(0),
                            )
                            .trim()
                            .to_string(),
                        ),
                    ] {
                        if !v.is_empty() {
                            s.push_str(&format!("{label}: {v}\r\n"));
                        }
                    }
                }
            }
            s.push_str("\r\n");
            // A message with only an HTML body used to say so and send you to export.
            // Now the markup comes off and the text is here — see `pstfree::html` for
            // what that keeps and what it knowingly throws away.
            match body_text(&pc) {
                Some(b) => s.push_str(&b.replace("\r\n", "\n").replace('\n', "\r\n")),
                None => s.push_str("(This message has no body.)"),
            }
            s
        }
    };

    let t = wide(&body);
    SendMessageW(app.text, WM_SETTEXT, 0, t.as_ptr() as LPARAM);
}

/// How much of the file an export covers. Everything is the old behaviour and still the
/// default; the other two are the thing anybody wants when the archive is 20GB and the
/// reason they opened it is one thread.
#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Everything,
    Folder,
    Message,
}

unsafe fn do_export(hwnd: HWND, app: &mut App, format: Format, scope: Scope) {
    if !ready(hwnd, app) {
        return;
    }

    // Worked out before the folder is picked, so a selection that is not there is refused
    // before anybody has browsed for somewhere to put the result.
    let (nodes, what) = match scope {
        Scope::Everything => (app.nodes.clone(), "Everything".to_string()),
        Scope::Folder => {
            let Some(name) = app.names.get(&app.folder).cloned() else {
                message_box(
                    hwnd,
                    "Select a folder in the tree first. A search result is not a folder — \
                     use \"Export this message\" for one of its hits.",
                    "pstfree",
                    MB_ICONINFORMATION,
                );
                return;
            };
            (
                export::subtree(&app.nodes, app.folder),
                format!("{name} and everything under it"),
            )
        }
        Scope::Message => {
            let sel = SendMessageW(
                app.list,
                LVM_GETNEXTITEM,
                usize::MAX,
                LVNI_SELECTED as LPARAM,
            );
            let Some(node) = usize::try_from(sel)
                .ok()
                .and_then(|i| app.shown.get(i))
                .copied()
            else {
                message_box(
                    hwnd,
                    "Select a message in the list first.",
                    "pstfree",
                    MB_ICONINFORMATION,
                );
                return;
            };
            // Every folder node is kept so the message still lands in the right place in
            // the exported tree rather than in _no-folder.
            let nodes = app
                .nodes
                .iter()
                .filter(|n| n.nid_type() == 0x02 || n.nid == node.nid)
                .copied()
                .collect();
            (nodes, "One message".to_string())
        }
    };

    let Some(dir) = pick_folder(hwnd) else { return };
    let root = std::path::PathBuf::from(&dir);
    if root.read_dir().is_ok_and(|mut d| d.next().is_some()) {
        message_box(
            hwnd,
            "That folder already has things in it. Choose an empty one — an export writes a lot of files.",
            "pstfree",
            MB_ICONWARNING,
        );
        return;
    }

    spawn_job(hwnd, app, "Exporting", move |pst, all, on| {
        // The worker reopened the file and got its own index; the names have to come from
        // that one, but which messages to write was decided against the one on screen.
        let names = folder_names(pst, &all);
        let st = export::export(pst, &nodes, &names, &root, format, on);
        let mut msg = format!("{what}: {} message(s) written to {dir}", st.messages);
        if st.attachments > 0 {
            msg += &format!("\n{} attachment(s) included.", st.attachments);
        }
        if st.failed > 0 {
            msg += &format!("\n{} could not be written.", st.failed);
        }
        Done::Text(msg)
    });
}

/// Write the open file back out as a clean PST. The thing the paid tools sell, and until
/// now the one thing here that the command line could do and the window could not.
unsafe fn do_rebuild(hwnd: HWND, app: &mut App) {
    if !ready(hwnd, app) {
        return;
    }
    let Some(out) = pick_save(hwnd) else { return };
    if std::path::Path::new(&out) == std::path::Path::new(&app.path) {
        message_box(
            hwnd,
            "Write the repair somewhere else. Repairing a file over itself is how a bad \
             day becomes an unrecoverable one.",
            "pstfree",
            MB_ICONWARNING,
        );
        return;
    }

    spawn_job(hwnd, app, "Repairing", move |pst, nodes, on| {
        let blocks = index_blocks(pst);
        Done::Text(
            match pstfree::repair::rebuild(pst, &nodes, &blocks, &out, on) {
                Err(e) => format!("Nothing was written.\n\n{e}"),
                Ok(r) => {
                    let mut msg = format!(
                        "Wrote {out}\n\n{} node(s), {} block(s), {} bytes.",
                        r.nodes, r.blocks, r.bytes
                    );
                    if r.converted {
                        msg += &format!(
                            "\n\nConverted from {}. Every data stream was decoded and laid out \
                         again as a Unicode PST stores them, so this is a new file rather \
                         than a copy of the old one — check it against the original before \
                         deleting anything.",
                            r.source
                        );
                    }
                    for p in &r.problems {
                        msg += &format!("\n\n{p}");
                    }
                    if r.dropped_blocks > 0 || r.dropped_nodes > 0 {
                        msg += &format!(
                            "\n\nLeft out {} block(s) that failed their own checksum, and {} \
                         node(s) whose data they held.",
                            r.dropped_blocks, r.dropped_nodes
                        );
                    }
                    if r.missing.is_empty() {
                        msg += "\n\nThe allocation maps go out marked invalid, which is the \
                            documented way to say 'rebuild these before writing'. Outlook \
                            does that on open, and reopening the file here will report \
                            that one thing on purpose.";
                    } else {
                        msg += &format!(
                            "\n\nThis file will NOT open: it has no {}. That node's data block \
                         did not survive, and no index can point at bytes that are gone. \
                         pstfree still reads the result, so export is the way to get this \
                         mail out.",
                            r.missing.join(", no ")
                        );
                    }
                    msg
                }
            },
        )
    });
}

/// Search the whole file and put the hits in the message list.
///
/// Subject, sender and body, one case-insensitive substring — the same `ltp::find` the
/// command line calls, so the two cannot drift. The results replace what the list is
/// showing and the Folder column is what says where each one came from; picking a folder
/// in the tree puts the ordinary view back.
///
/// A job like export and repair, because on a real mailbox it is: a body search reads
/// every message in the file, and 500,000 of them is minutes, not the instant the three
/// fixtures made it look like.
unsafe fn do_find(hwnd: HWND, app: &mut App) {
    if !ready(hwnd, app) {
        return;
    }

    let mut buf = [0u16; 256];
    let n = GetWindowTextW(app.search, buf.as_mut_ptr(), buf.len() as i32);
    let needle = from_wide(&buf[..n.max(0) as usize]);
    if needle.trim().is_empty() {
        SetFocus(app.search);
        set_status(hwnd, "Type something to search for, then press Enter.");
        return;
    }

    spawn_job(hwnd, app, "Searching", move |pst, nodes, on| {
        let hits = pstfree::ltp::find(pst, &nodes, &needle, on);
        Done::Found(needle, hits)
    });
}

/// A finished search, into the list. The hits' nodes came from the worker's own index on
/// the same file, so they are the ones the window's handle would have found.
unsafe fn show_hits(hwnd: HWND, app: &mut App, needle: &str, hits: &[pstfree::ltp::Hit]) {
    SendMessageW(app.list, LVM_DELETEALLITEMS, 0, 0);
    app.shown.clear();
    app.folder = 0;

    let mut in_body = 0;
    for (i, h) in hits.iter().enumerate() {
        if h.matched == "body" {
            in_body += 1;
        }
        let folder = app
            .names
            .get(&h.folder)
            .cloned()
            .unwrap_or_else(|| "(no folder)".into());
        add_row(app.list, i, &h.date, &h.sender, &h.subject, &folder);
        app.shown.push(h.node);
    }

    let t = wide("");
    SendMessageW(app.text, WM_SETTEXT, 0, t.as_ptr() as LPARAM);
    if hits.is_empty() {
        set_status(
            hwnd,
            &format!("Nothing in this file mentions \"{needle}\". Pick a folder to go back."),
        );
        return;
    }

    // Said out loud, because a hit that matched in the body shows nothing matching on its
    // row and otherwise looks like the search is broken.
    set_status(
        hwnd,
        &format!(
            "{} message(s) mention \"{needle}\"{}. Pick a folder in the tree to go back.",
            hits.len(),
            if in_body > 0 {
                format!(", {in_body} of them in the message body")
            } else {
                String::new()
            }
        ),
    );
    let mut sel: LVITEMW = std::mem::zeroed();
    sel.mask = LVIF_STATE;
    sel.state = LVIS_SELECTED | LVIS_FOCUSED;
    sel.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
    SendMessageW(app.list, LVM_SETITEMSTATE, 0, &sel as *const _ as LPARAM);
    show_message(app, 0);
}

/// Everything that was wrong with the file, in full, rather than as a count.
unsafe fn do_report(hwnd: HWND, app: &mut App) {
    if app.pst.is_none() {
        message_box(hwnd, "Open a file first.", "pstfree", MB_ICONINFORMATION);
        return;
    }
    let mut msg = String::new();
    if app.salvaged {
        msg += "The index in the header could not be read. The one in use was rebuilt by \
                sweeping the file for surviving B-tree pages and carving blocks out of the \
                file itself.\n\n";
    }
    if app.problems.is_empty() {
        msg += "Nothing else is wrong with this file. Every checksum in it verifies.";
    } else {
        msg += &format!("{} problem(s):\n", app.problems.len());
        for w in &app.problems {
            msg += &format!("\n  \u{2022} {w}");
        }
    }
    message_box(
        hwnd,
        &msg,
        "What is wrong with this file",
        MB_ICONINFORMATION,
    );
}

/// A file is open and nothing else is running. Both refusals say which it is.
unsafe fn ready(hwnd: HWND, app: &App) -> bool {
    if app.pst.is_none() {
        message_box(hwnd, "Open a file first.", "pstfree", MB_ICONINFORMATION);
        return false;
    }
    if app.busy {
        message_box(
            hwnd,
            "Something is already running. Wait for it to finish.",
            "pstfree",
            MB_ICONINFORMATION,
        );
        return false;
    }
    true
}

/// Run a long job on its own thread, against its own handle on the same file.
///
/// A rebuild or an export of a mailbox-sized PST takes minutes, and doing it on the
/// message loop is how a window ends up saying "Not Responding". The worker opens the
/// file a second time rather than borrowing the one on screen: it is opened read-only,
/// so a second handle costs nothing and removes every question about sharing the first.
unsafe fn spawn_job<F>(hwnd: HWND, app: &mut App, label: &'static str, job: F)
where
    F: FnOnce(&mut Pst, Vec<Node>, pstfree::Progress) -> Done + Send + 'static,
{
    let post = Poster(hwnd);
    let path = app.path.clone();
    app.busy = true;
    set_status(hwnd, &format!("{label}..."));

    // Closing the window mid-job leaks the last message's String, because there is no
    // longer a handler to take it back. The process is on its way out at that point, so
    // that is the whole of the cost.
    std::thread::spawn(move || {
        let done = match load(&path) {
            Err(e) => Done::Text(format!("{path} could not be reopened for this: {e}")),
            Ok((mut pst, nodes, _)) => {
                // Throttled here rather than in the library: every tick is a window
                // message, and a million of them would be the slow part of the job.
                let mut last = std::time::Instant::now();
                let mut on = |a: u64, b: u64| {
                    let now = std::time::Instant::now();
                    if now - last >= std::time::Duration::from_millis(150) {
                        last = now;
                        let pct = (a * 100).checked_div(b).unwrap_or(100);
                        post.say(format!("{label}: {a} of {b} ({pct}%)"));
                    }
                };
                job(&mut pst, nodes, &mut on)
            }
        };
        post.done(done);
    });
}
