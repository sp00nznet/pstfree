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
    clean_subject, filetime, read_node_pc, NID_ROOT_FOLDER, PID_DELIVERY_TIME, PID_DISPLAY_NAME,
    PID_SENDER_NAME, PID_SUBJECT, PID_SUBMIT_TIME, PID_TRANSPORT_HEADERS,
};
use pstfree::ndb::{Node, Pst};
use std::collections::BTreeMap;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateSolidBrush, GetStockObject, DEFAULT_GUI_FONT, HFONT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_TREE: isize = 1;
const ID_LIST: isize = 2;
const ID_TEXT: isize = 3;
const ID_OPEN: usize = 100;
const ID_EXPORT_EML: usize = 101;
const ID_EXPORT_MBOX: usize = 102;
const ID_EXPORT_MSG: usize = 103;
const ID_QUIT: usize = 104;

/// Everything the window needs to answer a message. Boxed once and hung off the window.
struct App {
    pst: Option<Pst>,
    path: String,
    nodes: Vec<Node>,
    /// Folder id -> its messages, newest first, as (node, subject, sender, date).
    by_folder: BTreeMap<u32, Vec<(Node, String, String, String)>>,
    /// What the list is currently showing.
    shown: Vec<Node>,
    tree: HWND,
    list: HWND,
    text: HWND,
    status: HWND,
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
            hbrBackground: CreateSolidBrush(0x00F0F0F0 as COLORREF),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);

        let title = wide("pstfree");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1100,
            720,
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

        ShowWindow(hwnd, SW_SHOW);
        let mut msg = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
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

unsafe fn create_children(hwnd: HWND) {
    let instance = GetModuleHandleW(std::ptr::null());
    let font = GetStockObject(DEFAULT_GUI_FONT) as HFONT;

    let mk = |class: &str, style: u32, id: isize| -> HWND {
        let c = wide(class);
        let h = CreateWindowExW(
            0,
            c.as_ptr(),
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE | style,
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
    let status = mk("msctls_statusbar32", 0, 0);

    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        0,
        LVS_EX_FULLROWSELECT as LPARAM,
    );
    for (i, (title, width)) in [("Date", 130), ("From", 190), ("Subject", 460)]
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
        tree,
        list,
        text,
        status,
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);

    let menu = CreateMenu();
    let file = CreateMenu();
    let items = [
        (ID_OPEN, "&Open PST or OST...\tCtrl+O"),
        (0, ""),
        (ID_EXPORT_EML, "Export all as &.eml..."),
        (ID_EXPORT_MBOX, "Export all as m&box..."),
        (ID_EXPORT_MSG, "Export all as .m&sg..."),
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

    set_status(
        hwnd,
        "Open a .pst or .ost file to begin. No password is ever needed.",
    );
}

/// Left third for the folders, the rest split between the message list and the message.
unsafe fn layout(hwnd: HWND, app: &App) {
    let mut rc = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);

    SendMessageW(app.status, WM_SIZE, 0, 0);
    let mut sr = std::mem::zeroed();
    GetWindowRect(app.status, &mut sr);
    let sh = sr.bottom - sr.top;

    let body = h - sh;
    let tree_w = (w / 3).min(340);
    let list_h = body / 2;
    MoveWindow(app.tree, 0, 0, tree_w, body, 1);
    MoveWindow(app.list, tree_w, 0, w - tree_w, list_h, 1);
    MoveWindow(app.text, tree_w, list_h, w - tree_w, body - list_h, 1);
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
        ID_EXPORT_EML => do_export(hwnd, app, Format::Eml),
        ID_EXPORT_MBOX => do_export(hwnd, app, Format::Mbox),
        ID_EXPORT_MSG => do_export(hwnd, app, Format::Msg),
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

unsafe fn open_file(hwnd: HWND, app: &mut App, path: &str) {
    set_status(hwnd, &format!("Reading {path}..."));
    let mut pst = match Pst::open(path) {
        Ok(p) => p,
        Err(e) => {
            message_box(hwnd, &e, "pstfree", MB_ICONERROR);
            set_status(hwnd, "Nothing open.");
            return;
        }
    };

    // Same fallback the command line uses: a file whose index is gone is the one someone
    // most wants opened, so it is rebuilt rather than refused.
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

    // Folder names, then every message grouped under the folder that holds it.
    let mut names = BTreeMap::new();
    for n in nodes.iter().filter(|n| n.nid_type() == 0x02) {
        let name = read_node_pc(&mut pst, n)
            .ok()
            .and_then(|pc| pc.str(PID_DISPLAY_NAME).map(str::to_string))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                if n.nid == NID_ROOT_FOLDER {
                    path.rsplit(['\\', '/'])
                        .next()
                        .unwrap_or("(root)")
                        .to_string()
                } else {
                    "(unnamed)".into()
                }
            });
        names.insert(n.nid, name);
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

    // Cleared before the tree is filled, not after: filling it selects a folder and
    // populates the list, and clearing afterwards would wipe exactly that.
    SendMessageW(app.list, LVM_DELETEALLITEMS, 0, 0);
    let t = wide("");
    SendMessageW(app.text, WM_SETTEXT, 0, t.as_ptr() as LPARAM);
    fill_tree(app, &names);

    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    set_status(
        hwnd,
        &format!(
            "{file} — {} folders, {total} messages{}{}",
            names.len(),
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

    let title = wide(&format!("pstfree — {file}"));
    SetWindowTextW(hwnd, title.as_ptr());
}

/// Fill the folder tree, following the node parent pointers.
unsafe fn fill_tree(app: &mut App, names: &BTreeMap<u32, String>) {
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

unsafe fn show_folder(app: &mut App, nid: u32) {
    SendMessageW(app.list, LVM_DELETEALLITEMS, 0, 0);
    app.shown.clear();
    let Some(msgs) = app.by_folder.get(&nid) else {
        return;
    };

    for (i, (node, subject, from, date)) in msgs.iter().enumerate() {
        let mut d = wide(date);
        let mut item: LVITEMW = std::mem::zeroed();
        item.mask = LVIF_TEXT;
        item.iItem = i as i32;
        item.pszText = d.as_mut_ptr();
        SendMessageW(app.list, LVM_INSERTITEMW, 0, &item as *const _ as LPARAM);

        for (col, s) in [(1, from), (2, subject)] {
            let mut t = wide(s);
            let mut sub: LVITEMW = std::mem::zeroed();
            sub.mask = LVIF_TEXT;
            sub.iItem = i as i32;
            sub.iSubItem = col;
            sub.pszText = t.as_mut_ptr();
            SendMessageW(app.list, LVM_SETITEMW, 0, &sub as *const _ as LPARAM);
        }
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
            match pc.str(pstfree::ltp::PID_BODY) {
                Some(b) if !b.trim().is_empty() => {
                    s.push_str(&b.replace("\r\n", "\n").replace('\n', "\r\n"))
                }
                _ => s.push_str(
                    "(This message has no plain-text body. Export it to see the HTML version.)",
                ),
            }
            s
        }
    };

    let t = wide(&body);
    SendMessageW(app.text, WM_SETTEXT, 0, t.as_ptr() as LPARAM);
}

unsafe fn do_export(hwnd: HWND, app: &mut App, format: Format) {
    if app.pst.is_none() {
        message_box(hwnd, "Open a file first.", "pstfree", MB_ICONINFORMATION);
        return;
    }
    let Some(dir) = pick_folder(hwnd) else { return };
    let root = std::path::Path::new(&dir);
    if root.read_dir().is_ok_and(|mut d| d.next().is_some()) {
        message_box(
            hwnd,
            "That folder already has things in it. Choose an empty one — an export writes a lot of files.",
            "pstfree",
            MB_ICONWARNING,
        );
        return;
    }

    set_status(hwnd, "Exporting...");
    let pst = app.pst.as_mut().unwrap();
    let mut names = BTreeMap::new();
    for n in app.nodes.iter().filter(|n| n.nid_type() == 0x02) {
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

    let st = export::export(pst, &app.nodes, &names, root, format);
    let msg = format!(
        "{} message(s) written to {dir}{}{}",
        st.messages,
        if st.attachments > 0 {
            format!("\n{} attachment(s) included.", st.attachments)
        } else {
            String::new()
        },
        if st.failed > 0 {
            format!("\n{} could not be written.", st.failed)
        } else {
            String::new()
        }
    );
    set_status(hwnd, &msg.replace('\n', " "));
    message_box(hwnd, &msg, "Export finished", MB_ICONINFORMATION);
}
