//! 原生贴图：一张图 = 一个分层窗口 = 一条自己的线程，不是一整个浏览器进程。
//!
//! 为什么值得单独一层：贴图的语义就是"把那块图摆在桌面上"，需要的全部能力是显示位图、
//! 拖动、滚轮换档、关掉。这四件事在 WebView2 那条路上每张要付一个渲染进程
//! （几十 MB 量级，上限 12 张），原生这条路上只付图像本身。
//!
//! 为什么"一张图一条线程"：窗口属于创建它的那条线程，消息只在那条线程被取出。
//! 共用一条线程就得先把"建窗"投递过去（指针装箱 + 自定义消息 + 超时之后那块内存归谁），
//! 而那是这一族最容易留下悬垂引用的写法。一条线程一张图多的是 N 份线程栈
//! （按需提交，几十 KB 量级），换掉的是那层投递，顺带去掉"必须回主线程建窗、
//! 还可能卡在派发栈里"那条 webview 约束（见 `super` 模块头）。
//!
//! **贴图必须不透明**（`super` 模块头第 2 条）：截图链路里 BGRA 那格的 alpha 是 0
//! （凑字节用的，不是透明度），原样交给分层窗口会得到一张完全看不见的贴图 ——
//! 窗口存在、点得到、屏幕上什么都没有。所以解码之后先把 alpha 钉成 255。
//!
//! 每窗状态放在 `GWLP_USERDATA` 里（下面 `state_of` 那段解释为什么不能放别处）。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi as g;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging as wm;

use crate::floating::render::px;

use super::{pin_size, zoom_step};

/// 右上角 ✕ 的边长与离边距离（逻辑像素）。命中判定与绘制共用同一个矩形，
/// 所以"看得见的叉"和"点得到的叉"是同一个数 —— 各算一遍正是"看得见点不到 /
/// 点得到看不见"那一族的来源。
const CLOSE_BOX_LOGICAL: i32 = 20;
const CLOSE_BOX_INSET_LOGICAL: i32 = 3;
const CLASS_NAME: &str = "FocusFlowPinWindow";

/// 序号 → 句柄。跨线程关窗（托盘的"关掉所有贴图"、命令）只认序号。
static LIVE: Mutex<Vec<(u32, isize)>> = Mutex::new(Vec::new());
static LIVE_COUNT: AtomicU32 = AtomicU32::new(0);

pub fn live_count() -> usize {
    LIVE_COUNT.load(Ordering::SeqCst) as usize
}

// ===== 纯函数部分（能在无头用例里钉死的那三件）=====

/// RGBA8（解码器给的顺序）→ 预乘 BGRA8，并把 alpha 钉成 255。
///
/// 拆出来的理由：换序 = 红蓝反了（"有颜色但不对劲"），alpha 忘了抬 = 整张透明
/// （"图根本看不见"）。两个症状都不报错，只能在用例里判。
/// 不透明 ⇒ 预乘就是原样，所以这里不需要再乘一遍。
pub fn to_premultiplied_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for p in rgba.as_chunks::<4>().0 {
        out.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    out
}

/// ✕ 按钮的客户区矩形（物理像素，右上角）。
pub fn close_box(client_w: i32, scale: f64) -> crate::floating::render::Rect {
    let side = px(CLOSE_BOX_LOGICAL, scale);
    let inset = px(CLOSE_BOX_INSET_LOGICAL, scale);
    crate::floating::render::Rect {
        x: client_w - side - inset,
        y: inset,
        w: side,
        h: side,
    }
}

/// 客户区坐标命中 ✕ 吗。
pub fn hits_close(x: i32, y: i32, client_w: i32, scale: f64) -> bool {
    let b = close_box(client_w, scale);
    x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h
}

fn paint_close_mark(buf: &mut [u8], w: i32, h: i32, scale: f64) {
    let b = close_box(w, scale);
    for y in b.y.max(0)..(b.y + b.h).min(h) {
        for x in b.x.max(0)..(b.x + b.w).min(w) {
            let i = (y * w + x) as usize * 4;
            buf[i] = 40;
            buf[i + 1] = 40;
            buf[i + 2] = 40;
            buf[i + 3] = 235;
        }
    }
    let pad = px(5, scale);
    let inner = (b.h - 2 * pad).max(0);
    let ink = px(2, scale).max(1);
    for t in 0..inner {
        let x = b.x + pad + t;
        for y in [b.y + pad + t, b.y + pad + (inner - 1 - t)] {
            for dy in 0..ink {
                let yy = y + dy;
                if x >= 0 && yy >= 0 && x < w && yy < h {
                    let i = (yy * w + x) as usize * 4;
                    buf[i] = 245;
                    buf[i + 1] = 245;
                    buf[i + 2] = 245;
                    buf[i + 3] = 255;
                }
            }
        }
    }
}

// ===== 位图 =====

/// 一块 32bpp 顶向 DIB。
struct Dib {
    bmp: g::HBITMAP,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Dib {
    /// # Safety
    /// 这块位图必须**没有**被任何 DC 选中着再调用：选中的 GDI 对象删不掉，
    /// 内存要等那个 DC 一起没，于是每滚一次轮就漏一块整屏位图。
    unsafe fn release(self, hdc: g::HDC) {
        let _ = g::SelectObject(hdc, g::HGDIOBJ(std::ptr::null_mut()));
        let _ = g::DeleteObject(g::HGDIOBJ(self.bmp.0));
    }
    fn pixels(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h * 4) as usize) }
    }
}

unsafe fn make_dib(hdc: g::HDC, w: i32, h: i32) -> Option<Dib> {
    let info = g::BITMAPINFO {
        bmiHeader: g::BITMAPINFOHEADER {
            biSize: std::mem::size_of::<g::BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: g::BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let bmp = g::CreateDIBSection(
        Some(hdc),
        &raw const info,
        g::DIB_RGB_COLORS,
        &mut bits,
        None,
        0,
    )
    .ok()?;
    if bits.is_null() {
        return None;
    }
    Some(Dib {
        bmp,
        bits: bits as *mut u8,
        w,
        h,
    })
}

// ===== 每窗状态 =====

/// 一张贴图在这条线程上的全部事实。
struct PinState {
    seq: u32,
    /// 1:1 的源位图（预乘 BGRA）。它归这条线程建、也归这条线程删。
    src: Dib,
    w: u32,
    h: u32,
    scale: f64,
}

fn scale_of(hwnd: HWND) -> f64 {
    unsafe { (GetDpiForWindow(hwnd).max(96)) as f64 / 96.0 }
}

/// 为什么放 `GWLP_USERDATA`：`wndproc` 是自由函数，只能从窗口本身拿到"这是哪一张"。
/// 另一个常见做法是把序号编进 `HWND` 的高位 —— 那是**错的**：句柄是内核对象，
/// 高位不是空闲位，编出来的那个数不再是合法句柄，销毁时会打在别的对象上。
/// 这里放的是 `Box<PinState>` 的裸指针，`WM_NCCREATE` 时从 `lpparam` 收下、
/// `WM_DESTROY` 时收回所有权，所以既没有泄漏也没有第二个观察者。
unsafe fn state_of(hwnd: HWND) -> Option<&'static mut PinState> {
    let raw = wm::GetWindowLongPtrW(hwnd, wm::GWLP_USERDATA) as *mut PinState;
    if raw.is_null() {
        None
    } else {
        Some(&mut *raw)
    }
}

/// 一次合成：1:1 源图 → 目标尺寸（带滤波拉伸）→ 加 ✕ → 交给 DWM。
unsafe fn paint(hwnd: HWND, st: &mut PinState) -> bool {
    let (tw, th) = match pin_size(st.w, st.h, st.scale) {
        Ok((w, h)) => (w as i32, h as i32),
        Err(e) => {
            tracing::warn!("贴图算不出目标尺寸：{e}");
            return false;
        }
    };
    let screen = g::GetDC(Some(hwnd));
    if screen.0.is_null() {
        tracing::warn!("贴图取不到屏幕 DC，这一帧没画上");
        return false;
    }
    let mem = g::CreateCompatibleDC(Some(screen));
    let src_dc = g::CreateCompatibleDC(Some(screen));
    if mem.0.is_null() || src_dc.0.is_null() {
        if !mem.0.is_null() {
            let _ = g::DeleteDC(mem);
        }
        if !src_dc.0.is_null() {
            let _ = g::DeleteDC(src_dc);
        }
        let _ = g::ReleaseDC(Some(hwnd), screen);
        tracing::warn!("贴图建内存 DC 失败，这一帧没画上");
        return false;
    }
    let Some(mut dst) = make_dib(mem, tw, th) else {
        let _ = g::DeleteDC(mem);
        let _ = g::DeleteDC(src_dc);
        let _ = g::ReleaseDC(Some(hwnd), screen);
        tracing::warn!("贴图建目标位图失败（{tw}x{th}），这一帧没画上");
        return false;
    };
    let _ = g::SelectObject(src_dc, g::HGDIOBJ(st.src.bmp.0));
    let _ = g::SelectObject(mem, g::HGDIOBJ(dst.bmp.0));
    // HALFTONE = 带滤波的缩放。默认抽样在把整屏截图缩到 25% 时会把文字变成噪点。
    let _ = g::SetStretchBltMode(mem, g::STRETCH_HALFTONE);
    let mut ok = g::StretchBlt(
        mem,
        0,
        0,
        tw,
        th,
        Some(src_dc),
        0,
        0,
        st.w as i32,
        st.h as i32,
        g::SRCCOPY,
    )
    .as_bool();
    if ok {
        paint_close_mark(dst.pixels(), tw, th, scale_of(hwnd));
        let blend = g::BLENDFUNCTION {
            BlendOp: g::AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: g::AC_SRC_ALPHA as u8,
        };
        let size = SIZE { cx: tw, cy: th };
        let at = POINT { x: 0, y: 0 };
        ok = wm::UpdateLayeredWindow(
            hwnd,
            None,
            None,
            Some(&raw const size),
            Some(mem),
            Some(&raw const at),
            COLORREF(0),
            Some(&raw const blend),
            wm::ULW_ALPHA,
        )
        .is_ok();
    }
    if !ok {
        tracing::warn!("贴图这一帧没画上（{tw}x{th}）：StretchBlt 或交给 DWM 那一步失败了");
    }
    let _ = g::SelectObject(mem, g::HGDIOBJ(std::ptr::null_mut()));
    let _ = g::SelectObject(src_dc, g::HGDIOBJ(std::ptr::null_mut()));
    // `dst` 必须先摘下来再删（见 `Dib::release` 的 Safety），源位图留给线程退出时删
    dst.release(mem);
    let _ = g::DeleteDC(mem);
    let _ = g::DeleteDC(src_dc);
    let _ = g::ReleaseDC(Some(hwnd), screen);
    ok
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        wm::WM_NCCREATE => {
            // 状态盒子的所有权在这里从 `lpparam` 接过来，写进窗口属性。
            if let Some(cs) = (lparam.0 as *mut wm::CREATESTRUCTW).as_ref() {
                wm::SetWindowLongPtrW(hwnd, wm::GWLP_USERDATA, cs.lpCreateParams as isize);
            }
            wm::DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        wm::WM_NCHITTEST => {
            // 整张图都要能拖，唯独右上角那枚 ✕ 要能点：默认 HTCAPTION，命中 ✕ 交回 HTCLIENT。
            //
            // 这里不用 `ScreenToClient` 而是拿 `GetWindowRect` 自己减：这种窗口是
            // `WS_POPUP` + 无边界 + 分层，客户区矩形与窗口矩形本来就重合，
            // 少一个 API 依赖就少一处签名会记错的地方（同一条理由见 `scale_of`）。
            let sx = ((lparam.0 & 0xFFFF) as u16) as i16 as i32;
            let sy = ((lparam.0 >> 16) as u16) as i16 as i32;
            let mut rc = RECT::default();
            if wm::GetClientRect(hwnd, &mut rc).is_ok() {
                let mut wr = RECT::default();
                if wm::GetWindowRect(hwnd, &mut wr).is_ok() {
                    let cx = sx - wr.left;
                    let cy = sy - wr.top;
                    if cx >= 0 && cy >= 0 && hits_close(cx, cy, rc.right, scale_of(hwnd)) {
                        return LRESULT(wm::HTCLIENT as isize);
                    }
                }
            }
            LRESULT(wm::HTCAPTION as isize)
        }
        wm::WM_LBUTTONUP => {
            // 只有命中 ✕ 才会收到客户区消息（别处都被系统当成拖动处理了）
            let _ = wm::DestroyWindow(hwnd);
            LRESULT(0)
        }
        wm::WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) as u16) as i16;
            let dir = if delta > 0 {
                1
            } else if delta < 0 {
                -1
            } else {
                0
            };
            if let Some(st) = state_of(hwnd) {
                let next = zoom_step(st.scale, dir);
                if (next - st.scale).abs() > f64::EPSILON {
                    st.scale = next;
                    if let Ok((w, h)) = pin_size(st.w, st.h, st.scale) {
                        let mut rc = RECT::default();
                        let _ = wm::GetWindowRect(hwnd, &mut rc);
                        // 先改窗口尺寸再重画：反过来的话有一帧是"新图配旧框"
                        let _ = wm::SetWindowPos(
                            hwnd,
                            None,
                            rc.left,
                            rc.top,
                            w as i32,
                            h as i32,
                            wm::SWP_NOZORDER | wm::SWP_NOACTIVATE,
                        );
                        let _ = paint(hwnd, st);
                    }
                }
            }
            LRESULT(0)
        }
        wm::WM_KEYDOWN => {
            if wparam.0 == 0x1B
            /* VK_ESCAPE */
            {
                let _ = wm::DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        wm::WM_DPICHANGED => {
            if let Some(rc) = (lparam.0 as *const RECT).as_ref() {
                let _ = wm::SetWindowPos(
                    hwnd,
                    None,
                    rc.left,
                    rc.top,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    wm::SWP_NOZORDER | wm::SWP_NOACTIVATE,
                );
            }
            if let Some(st) = state_of(hwnd) {
                let _ = paint(hwnd, st);
            }
            LRESULT(0)
        }
        wm::WM_CLOSE => {
            let _ = wm::DestroyWindow(hwnd);
            LRESULT(0)
        }
        wm::WM_DESTROY => {
            let seq = state_of(hwnd).map(|s| s.seq).unwrap_or(0);
            {
                let mut guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());
                let before = guard.len();
                guard.retain(|(s, _)| *s != seq);
                if guard.len() != before {
                    LIVE_COUNT.store(guard.len() as u32, Ordering::SeqCst);
                    super::note_native_pin_gone(seq);
                }
            }
            // 收回 `WM_NCCREATE` 那步交出去的所有权。漏这一步就是一张整屏位图/每张贴图。
            let raw = wm::SetWindowLongPtrW(hwnd, wm::GWLP_USERDATA, 0) as *mut PinState;
            if !raw.is_null() {
                let boxed = unsafe { Box::from_raw(raw) };
                let tmp = g::CreateCompatibleDC(None);
                unsafe { boxed.src.release(tmp) };
                let _ = g::DeleteDC(tmp);
            }
            wm::PostQuitMessage(0);
            LRESULT(0)
        }
        _ => wm::DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ===== 线程 =====

fn register_class_once() -> Result<(), String> {
    // 0 没试过 / 2 正在注册 / 1 成功 / 3 失败。窗口类按模块句柄共享，
    // 第二条线程再注册同一个类名会得到 ERROR_CLASS_ALREADY_EXISTS（返回 0），
    // 所以只能让一个赢家去注册，其余人等它。
    static DONE: AtomicU32 = AtomicU32::new(0);
    let got = DONE.load(Ordering::SeqCst);
    if got == 1 {
        return Ok(());
    }
    if got == 3 {
        return Err("之前注册贴图窗口类就失败了".to_string());
    }
    if DONE
        .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        let r = unsafe { do_register() };
        DONE.store(if r.is_ok() { 1 } else { 3 }, Ordering::SeqCst);
        return r;
    }
    for _ in 0..500 {
        match DONE.load(Ordering::SeqCst) {
            1 => return Ok(()),
            3 => return Err("另一条线程注册贴图窗口类失败".to_string()),
            _ => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    Err("等另一条线程注册贴图窗口类超时".to_string())
}

unsafe fn do_register() -> Result<(), String> {
    let hmodule = GetModuleHandleW(None).map_err(|e| format!("取模块句柄失败：{e}"))?;
    let class: Vec<u16> = CLASS_NAME.encode_utf16().chain([0]).collect();
    let cursor = wm::LoadCursorW(None, wm::IDC_ARROW).unwrap_or_default();
    let wc = wm::WNDCLASSEXW {
        cbSize: std::mem::size_of::<wm::WNDCLASSEXW>() as u32,
        style: wm::CS_HREDRAW | wm::CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: hmodule.into(),
        hCursor: cursor,
        lpszClassName: windows::core::PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    if wm::RegisterClassExW(&raw const wc) == 0 {
        return Err(format!(
            "注册贴图窗口类失败（最后错误 {}）",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        ));
    }
    Ok(())
}

/// 起一张原生贴图。返回 `Ok(())` 说的是**已经画上第一帧**，不是"窗口存在"——
/// 与覆盖层那条路同一个口径：建起来但画不上，用户面前什么都没有。
pub fn open(seq: u32, rgba: Vec<u8>, w: u32, h: u32, x: i32, y: i32) -> Result<(), String> {
    register_class_once()?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
    std::thread::Builder::new()
        .name(format!("pin-native-{seq}"))
        .spawn(move || run_pin(seq, rgba, w, h, x, y, &tx))
        .map_err(|e| format!("启动贴图线程失败：{e}"))?;
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| format!("贴图线程没有回话（{e}）"))?
}

fn run_pin(
    seq: u32,
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    x: i32,
    y: i32,
    tx: &std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let pixels = to_premultiplied_bgra(&rgba);
    unsafe {
        let hmodule = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(format!("取模块句柄失败：{e}")));
                return;
            }
        };
        // 源位图：1:1 那一张，整条线程只建这一次，缩放靠 StretchBlt 而不是重解码
        let prep = g::CreateCompatibleDC(None);
        let src = make_dib(prep, w as i32, h as i32).inspect(|d| {
            let n = pixels.len().min((d.w * d.h * 4) as usize);
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), d.bits, n);
        });
        let _ = g::DeleteDC(prep);
        let Some(src) = src else {
            let _ = tx.send(Err("给贴图建源位图失败".to_string()));
            return;
        };
        let class: Vec<u16> = CLASS_NAME.encode_utf16().chain([0]).collect();
        let caption: Vec<u16> = format!("FocusFlow 贴图 #{seq}")
            .encode_utf16()
            .chain([0])
            .collect();
        let boxed = Box::new(PinState {
            seq,
            src,
            w,
            h,
            scale: 1.0,
        });
        // `Box::into_raw` 的所有权交给窗口：`WM_NCCREATE` 收下、`WM_DESTROY` 收回
        let payload = Box::into_raw(boxed) as *const core::ffi::c_void;
        let hwnd = match wm::CreateWindowExW(
            wm::WINDOW_EX_STYLE(wm::WS_EX_LAYERED.0 | wm::WS_EX_TOOLWINDOW.0),
            windows::core::PCWSTR(class.as_ptr()),
            windows::core::PCWSTR(caption.as_ptr()),
            wm::WS_POPUP,
            x,
            y,
            w as i32,
            h as i32,
            None,
            None,
            Some(hmodule.into()),
            Some(payload),
        ) {
            Ok(h) if !h.0.is_null() => h,
            Ok(_) => {
                let _ = tx.send(Err("CreateWindowExW 返回空句柄".to_string()));
                return;
            }
            Err(e) => {
                // 窗口根本没建起来 ⇒ 不会有 `WM_DESTROY` 来收这个盒子，这里自己收，
                // 否则每次建窗失败都稳定漏一张整屏位图。
                drop(Box::from_raw(payload as *mut PinState));
                let _ = tx.send(Err(format!("创建原生贴图窗口失败：{e}")));
                return;
            }
        };
        {
            let mut guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());
            guard.push((seq, hwnd.0 as isize));
            LIVE_COUNT.store(guard.len() as u32, Ordering::SeqCst);
        }
        let painted = match state_of(hwnd) {
            Some(st) => paint(hwnd, st),
            None => false,
        };
        if !painted {
            let _ = wm::DestroyWindow(hwnd);
            let _ = tx.send(Err("原生贴图建起来了但一帧都没画上".to_string()));
            return;
        }
        let _ = wm::ShowWindow(hwnd, wm::SW_SHOWNOACTIVATE);
        let _ = tx.send(Ok(()));

        let mut msg = wm::MSG::default();
        loop {
            let got = wm::GetMessageW(&raw mut msg, None, 0, 0);
            if got.0 == 0 {
                break; // WM_QUIT（由 WM_DESTROY 发）
            }
            if got.0 == -1 {
                tracing::error!("贴图 pin-{seq} 的消息循环被中断，这张图停在最后一帧");
                break;
            }
            let _ = wm::TranslateMessage(&raw const msg);
            let _ = wm::DispatchMessageW(&raw const msg);
        }
        // 走到这里窗口已经不在了（`WM_DESTROY` 已经收回盒子并 PostQuitMessage）。
        // 只有一种情况例外：循环被 -1 中断，那时盒子还挂在窗口属性上 ——
        // 那句注释在 `WM_DESTROY` 里，两处共用同一个所有权规则。
        let raw = wm::GetWindowLongPtrW(hwnd, wm::GWLP_USERDATA) as *mut PinState;
        if !raw.is_null() {
            let boxed = Box::from_raw(raw);
            let tmp = g::CreateCompatibleDC(None);
            boxed.src.release(tmp);
            let _ = g::DeleteDC(tmp);
            let _ = wm::SetWindowLongPtrW(hwnd, wm::GWLP_USERDATA, 0);
            let mut guard = LIVE.lock().unwrap_or_else(|e| e.into_inner());
            let before = guard.len();
            guard.retain(|(s, _)| *s != seq);
            if guard.len() != before {
                LIVE_COUNT.store(guard.len() as u32, Ordering::SeqCst);
                super::note_native_pin_gone(seq);
            }
        }
    }
}

/// 关掉一张：只发消息，真正的销毁在拥有它的那条线程上做。
pub fn close(seq: u32) -> bool {
    let raw = LIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(s, _)| *s == seq)
        .map(|(_, h)| *h);
    match raw {
        Some(r) => unsafe {
            wm::PostMessageW(Some(HWND(r as *mut _)), wm::WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok()
        },
        None => false,
    }
}

/// 关掉所有贴图，返回真的投出去几条。
pub fn close_all() -> usize {
    let ids: Vec<u32> = LIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(s, _)| *s)
        .collect();
    ids.iter().filter(|s| close(**s)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RGBA → BGRA 换序 + alpha 钉成 255。
    /// 这两个症状都不报错：换序 = "颜色有但不对劲"，alpha 没抬 = "整张透明、看不见"。
    /// 截图链路交出来的 alpha 就是 0（BGRA 凑字节），所以这一条不是假想敌。
    #[test]
    fn swaps_to_bgra_and_pins_alpha_opaque() {
        let rgba = [10u8, 20, 30, 0, 250, 128, 7, 255];
        let out = to_premultiplied_bgra(&rgba);
        assert_eq!(out, vec![30, 20, 10, 255, 7, 128, 250, 255]);
    }

    #[test]
    fn conversion_keeps_length_and_ignores_a_stray_tail() {
        assert_eq!(to_premultiplied_bgra(&[]).len(), 0);
        // 不是 4 的倍数：按四个一组的迭代丢掉不成组的尾巴字节，而不是 panic 或写出半个像素
        assert_eq!(to_premultiplied_bgra(&[1, 2, 3, 4, 5]).len(), 4);
    }

    /// ✕ 必须整个落在客户区里，且中心点绝不命中 ——
    /// 命中区跑到图外面，用户就会在桌面上"隔空"关掉贴图；反过来 ✕ 点不到，
    /// 一张关不掉的贴图就一直盖在桌面上（模块头把这条列为最烦的事）。
    #[test]
    fn the_close_box_sits_inside_and_only_the_corner_is_hit() {
        let (w, scale) = (800, 1.0);
        let b = close_box(w, scale);
        assert_eq!(b.x + b.w, w - CLOSE_BOX_INSET_LOGICAL, "右边只留那一条内缩");
        assert!(b.x > 0 && b.y >= 0 && b.w > 0);
        assert!(hits_close(b.x + 2, b.y + 2, w, scale), "叉里面要点得到");
        assert!(
            !hits_close(b.x - 1, b.y + 2, w, scale),
            "叉外一格不许算命中，否则整张图的左上角都成了关闭区"
        );
        assert!(
            !hits_close(400, 400, w, scale),
            "图的正中是拖动区，不是关闭区"
        );
        assert!(!hits_close(w, 0, w, scale), "客户区外一格也不算");
    }

    /// 150% 缩放下矩形要跟着长（`px` 是逐值四舍五入，不是把 scale 先取整）。
    #[test]
    fn the_close_box_scales_per_dimension() {
        let b = close_box(1200, 1.5);
        assert_eq!(b.w, 30, "20 × 1.5");
        assert_eq!(b.x, 1200 - 30 - 5, "内缩也要按同一把尺子放大");
    }

    /// 原生那条路**不进** `PENDING`/`DIMS`：那张槽是给页面重载用的，原生窗口没有页面。
    /// 留了就是每张白占几 MB base64，而"底图超过 8 份被收回"那条语义会莫名多出一些乘客。
    /// 钉的是顺序：解码、开窗、返回都必须排在往槽里塞东西之前。
    #[test]
    fn the_native_path_never_touches_the_page_slots() {
        let src = include_str!("../pin.rs");
        let body: String = src
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let at = body
            .find("if as_native {")
            .expect("open() 里没有原生那条分支");
        let rest = &body[at..];
        let ret = rest.find("return Ok(seq)").expect("原生那条分支没有返回");
        let first_slot = rest
            .find("slot_put(")
            .expect("原生分支之后应该还有 webview 那条塞槽的路");
        assert!(
            ret < first_slot,
            "原生那条路先塞了槽：它没有页面会来取，留着就是白占几 MB"
        );
    }

    /// 名额必须两边各数各的。写成"只数 webview_windows()"的话，原生贴图一张都不占名额，
    /// 上限 12 就成了摆设 —— 而这条上限存在的全部理由就是"每张是一个进程"。
    #[test]
    fn the_cap_counts_whichever_side_the_pins_live_on() {
        let src = include_str!("../pin.rs");
        let flat: String = src
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            flat.contains("letalive=ifas_native{native::live_count()}else{live_pins(app).len()}"),
            "名额不是按渲染器分着数的：原生贴图会不占上限"
        );
        assert!(
            flat.contains("ifrenderer_is_native()") && flat.contains("native::close_all()"),
            "「关掉所有贴图」没走原生那张表：设置页那条出口在原生模式下会一张都关不掉"
        );
    }
}
