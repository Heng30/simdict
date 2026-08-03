//! simdict —— Bing 词典查词小工具。
//!
//! UI 层基于纯 Rust 栈：egui（即时模式 GUI）+ egui_software_backend
//! （CPU 软渲染）+ x11rb（X11 协议，不链接 libxcb、不 dlopen），可 musl 全静态编译。
//! 中文输入走 XIM（fcitx5）。

mod translation;
use anyhow::Result;
use egui::{
    Key, Modifiers, PointerButton, Pos2, RawInput, RichText, Vec2, ViewportId, ViewportInfo,
};
use log::{info, warn};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        self, ConnectionExt as _, EventMask, Gcontext, ImageFormat, KeyPressEvent, PropMode,
        Window, WindowClass,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};
use xim::{Client, ClientError, ClientHandler, x11rb::X11rbClient};
use xim_parser::{AttributeName, ForwardEventFlag, InputStyle, Point};

const WIN_WIDTH: u16 = 600;
const WIN_HEIGHT: u16 = 400;

// 内嵌字体：CJK（Source Han Sans CN）+ IPA 音标（DejaVu Sans）
const CJK_FONT: &[u8] = include_bytes!("../SourceHanSansCN.otf");
const IPA_FONT: &[u8] = include_bytes!("../DejaVuSans.ttf");

/// 翻译结果字体大小（px）
const TRANSLATION_FONT_SIZE: f32 = 24.0;
/// 输入框字体大小（px）
const INPUT_FONT_SIZE: f32 = 28.0;
/// 文本内边距（px）
const TEXT_PADDING: f32 = 4.0;

// X11 keysym 常量
const KS_RETURN: u32 = 0xFF0D;
const KS_ESCAPE: u32 = 0xFF1B;
const KS_BACKSPACE: u32 = 0xFF08;
const KS_TAB: u32 = 0xFF09;
const KS_DELETE: u32 = 0xFFFF;
const KS_LEFT: u32 = 0xFF51;
const KS_UP: u32 = 0xFF52;
const KS_RIGHT: u32 = 0xFF53;
const KS_DOWN: u32 = 0xFF54;
const KS_HOME: u32 = 0xFF50;
const KS_END: u32 = 0xFF57;
const KS_PAGE_UP: u32 = 0xFF55;
const KS_PAGE_DOWN: u32 = 0xFF56;

fn main() -> Result<()> {
    // reqwest 使用 rustls-no-provider，运行时必须安装 crypto provider
    _ = rustls::crypto::ring::default_provider().install_default();
    env_logger::init();

    let initial_search = std::env::args()
        .nth(1)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let (conn, screen_num) = x11rb::connect(None).unwrap_or_else(|e| {
        eprintln!("无法连接 X server（需要图形环境）：{e}");
        std::process::exit(1);
    });
    let screen = &conn.setup().roots[screen_num];

    let (tx, rx) = mpsc::channel::<Result<String>>();
    let mut app = App::new(&conn, screen, screen_num, tx, initial_search)?;
    if !app.input.is_empty() {
        app.do_search();
    }

    let start = Instant::now();
    let ctx = egui::Context::default();
    setup_fonts(&ctx);
    ctx.set_visuals(egui::Visuals::dark());
    let mut renderer = egui_software_backend::EguiSoftwareRender::new(
        egui_software_backend::ColorFieldOrder::Bgra,
    );

    info!("window {}x{} ready", app.width, app.height);

    while app.running {
        let had_event = app.pump(&ctx, &rx)?;
        if had_event || app.first_frame || ctx.has_requested_repaint() {
            app.run_frame(&ctx, &mut renderer, &start)?;
            app.conn.flush()?;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "source_han_sans_cn".to_owned(),
        egui::FontData::from_static(CJK_FONT).into(),
    );
    fonts.font_data.insert(
        "deja_vu_sans".to_owned(),
        egui::FontData::from_static(IPA_FONT).into(),
    );
    // 回退顺序：默认字体（拉丁）→ DejaVu（IPA）→ Source Han Sans（CJK）
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let f = fonts.families.entry(family).or_default();
        f.push("deja_vu_sans".to_owned());
        f.push("source_han_sans_cn".to_owned());
    }
    // 专用字体族：音标行整体用 DejaVu Sans，避免拉丁字形混排
    fonts.families.insert(
        egui::FontFamily::Name("ipa".into()),
        vec!["deja_vu_sans".to_owned()],
    );
    ctx.set_fonts(fonts);
}

/// 中央面板的视图状态。
#[derive(PartialEq)]
enum State {
    Idle,
    Searching,
    NotFound,
    Result(String),
}

struct App<'a> {
    conn: &'a RustConnection,
    input: String,
    state: State,
    first_frame: bool,
    running: bool,
    tx: mpsc::Sender<Result<String>>,
    events: Vec<egui::Event>,

    window: Window,
    gc: Gcontext,
    depth: u8,
    width: u16,
    height: u16,
    pixels: Vec<[u8; 4]>,

    wm_protocols: xproto::Atom,
    wm_delete_window: xproto::Atom,

    first_keycode: u8,
    keysyms_per_keycode: u8,
    keymap: Vec<u32>,

    // XIM 输入法（fcitx5）：None = 无输入法环境，降级为纯键盘输入
    ime: Option<X11rbClient<&'a RustConnection>>,
    ime_state: Ime,
}

impl<'a> App<'a> {
    fn new(
        conn: &'a RustConnection,
        screen: &xproto::Screen,
        screen_num: usize,
        tx: mpsc::Sender<Result<String>>,
        initial_search: String,
    ) -> Result<Self> {
        let window = conn.generate_id()?;
        let gc = conn.generate_id()?;

        conn.create_window(
            screen.root_depth,
            window,
            screen.root,
            0,
            0,
            WIN_WIDTH,
            WIN_HEIGHT,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &xproto::CreateWindowAux::new().event_mask(
                EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::FOCUS_CHANGE,
            ),
        )?;
        conn.create_gc(gc, window, &xproto::CreateGCAux::new())?;

        let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
        let wm_delete_window = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
        let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let wm_name = conn.intern_atom(false, b"WM_NAME")?.reply()?.atom;
        let string_atom = conn.intern_atom(false, b"STRING")?.reply()?.atom;
        let atom_atom = conn.intern_atom(false, b"ATOM")?.reply()?.atom;

        // 悬浮窗口：UTILITY 类型（平铺 WM 不拉伸）+ 置顶 + 跳过任务栏 + 无装饰
        let net_wm_window_type = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE")?
            .reply()?
            .atom;
        let net_wm_window_type_utility = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE_UTILITY")?
            .reply()?
            .atom;
        let net_wm_state = conn.intern_atom(false, b"_NET_WM_STATE")?.reply()?.atom;
        let net_wm_state_above = conn
            .intern_atom(false, b"_NET_WM_STATE_ABOVE")?
            .reply()?
            .atom;
        let net_wm_state_skip_taskbar = conn
            .intern_atom(false, b"_NET_WM_STATE_SKIP_TASKBAR")?
            .reply()?
            .atom;
        let motif_wm_hints = conn.intern_atom(false, b"_MOTIF_WM_HINTS")?.reply()?.atom;
        let motif_wm_hints_type = conn.intern_atom(false, b"MOTIF_WM_HINTS")?.reply()?.atom;

        conn.change_property32(
            PropMode::REPLACE,
            window,
            net_wm_window_type,
            atom_atom,
            &[net_wm_window_type_utility],
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            net_wm_state,
            atom_atom,
            &[net_wm_state_above, net_wm_state_skip_taskbar],
        )?;
        // _MOTIF_WM_HINTS: [flags=MWM_HINTS_DECORATIONS, functions=0, decorations=0, input_mode=0, status=0]
        conn.change_property32(
            PropMode::REPLACE,
            window,
            motif_wm_hints,
            motif_wm_hints_type,
            &[2, 0, 0, 0, 0],
        )?;

        // EWMH：_NET_WM_STATE 必须用 ClientMessage 请求 WM 修改（直接写属性会被覆盖）
        conn.send_event(
            false,
            screen.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            xproto::ClientMessageEvent {
                response_type: 33, // ClientMessage
                format: 32,
                sequence: 0,
                window,
                type_: net_wm_state,
                data: [net_wm_state_above, net_wm_state_skip_taskbar, 0, 0, 0].into(),
            },
        )?;

        // 窗口标题 + 关闭协议
        conn.change_property8(
            PropMode::REPLACE,
            window,
            net_wm_name,
            utf8_string,
            b"simdict",
        )?;
        conn.change_property8(PropMode::REPLACE, window, wm_name, string_atom, b"simdict")?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            wm_protocols,
            atom_atom,
            &[wm_delete_window],
        )?;

        conn.map_window(window)?;
        conn.flush()?;

        let mut app = Self {
            conn,
            input: initial_search,
            state: State::Idle,
            first_frame: true,
            running: true,
            tx,
            events: Vec::new(),
            window,
            gc,
            depth: screen.root_depth,
            width: WIN_WIDTH,
            height: WIN_HEIGHT,
            pixels: vec![[0u8; 4]; WIN_WIDTH as usize * WIN_HEIGHT as usize],
            wm_protocols,
            wm_delete_window,
            first_keycode: conn.setup().min_keycode,
            keysyms_per_keycode: 0,
            keymap: Vec::new(),
            ime: None,
            ime_state: Ime::new(window),
        };
        app.load_keymap()?;
        app.ime = match X11rbClient::init(conn, screen_num, None) {
            Ok(ime) => {
                info!("XIM 客户端已初始化");
                Some(ime)
            }
            Err(e) => {
                warn!("无 XIM 输入法（{e}），中文输入不可用");
                None
            }
        };
        Ok(app)
    }

    fn load_keymap(&mut self) -> Result<()> {
        let min = self.conn.setup().min_keycode;
        let count = self.conn.setup().max_keycode - min + 1;
        let reply = self.conn.get_keyboard_mapping(min, count)?.reply()?;
        self.keysyms_per_keycode = reply.keysyms_per_keycode;
        self.keymap = reply.keysyms;
        Ok(())
    }

    /// 处理一轮 X 事件与异步结果，返回是否有事件发生。
    fn pump(&mut self, ctx: &egui::Context, rx: &mpsc::Receiver<Result<String>>) -> Result<bool> {
        let mut had_event = false;
        while let Some(event) = self.conn.poll_for_event()? {
            had_event = true;
            self.on_x_event(ctx, event)?;
        }
        if let Ok(res) = rx.try_recv() {
            self.state = match res {
                Ok(t) if t == translation::NOT_FOUND => {
                    info!("未找到：无匹配结果");
                    State::NotFound
                }
                Ok(t) => State::Result(t),
                Err(e) => State::Result(format!("Error: {e}")),
            };
            ctx.request_repaint();
        }
        Ok(had_event)
    }

    /// 单个 X 事件：XIM 优先，其次 IME 产物、按键转发、普通处理、焦点同步。
    fn on_x_event(&mut self, ctx: &egui::Context, event: x11rb::protocol::Event) -> Result<()> {
        // 1. XIM 协议事件优先（握手、commit、回传按键等）
        if let Some(im) = self.ime.as_mut() {
            if im.filter_event(&event, &mut self.ime_state)? {
                return Ok(());
            }
        }

        // 2. XIM 回调产物：commit 注入 egui 事件流（光标/撤销交给 TextEdit）；
        //    回传按键作为正常输入
        for text in std::mem::take(&mut self.ime_state.commits) {
            info!("IME commit: {text}");
            self.events.push(egui::Event::Text(text));
            ctx.request_repaint();
        }
        for xev in std::mem::take(&mut self.ime_state.forwarded) {
            self.handle_key(xev.detail, u16::from(xev.state), xev.response_type == 2)?;
        }

        // 3. 按键事件：IME 就绪时转发给 fcitx，由它决定消费或回传
        if let x11rb::protocol::Event::KeyPress(e) | x11rb::protocol::Event::KeyRelease(e) = &event
        {
            if let (Some(im), true) = (self.ime.as_mut(), self.ime_state.ready) {
                im.forward_event(
                    self.ime_state.im_id,
                    self.ime_state.ic_id,
                    ForwardEventFlag::empty(),
                    e,
                )?;
                return Ok(());
            }
        }

        // 4. 普通事件处理（先提取焦点信息，因为 event 会被移动）
        let focus_change = match &event {
            x11rb::protocol::Event::FocusIn(_) => Some(true),
            x11rb::protocol::Event::FocusOut(_) => Some(false),
            _ => None,
        };
        self.handle_x_event(event)?;

        // 5. 焦点变化同步给 IME
        if let (Some(focused), true) = (focus_change, self.ime_state.ready) {
            let im = self.ime.as_mut().expect("ime ready 时必存在");
            if focused {
                im.set_focus(self.ime_state.im_id, self.ime_state.ic_id)?;
            } else {
                im.unset_focus(self.ime_state.im_id, self.ime_state.ic_id)?;
            }
        }
        Ok(())
    }

    fn handle_x_event(&mut self, event: x11rb::protocol::Event) -> Result<()> {
        use x11rb::protocol::Event as X;
        match event {
            X::KeyPress(e) => self.handle_key(e.detail, u16::from(e.state), true)?,
            X::KeyRelease(e) => self.handle_key(e.detail, u16::from(e.state), false)?,
            X::ButtonPress(e) => {
                self.handle_button(u16::from(e.state), e.event_x, e.event_y, e.detail, true)
            }
            X::ButtonRelease(e) => {
                self.handle_button(u16::from(e.state), e.event_x, e.event_y, e.detail, false)
            }
            X::MotionNotify(e) => {
                self.events.push(egui::Event::PointerMoved(Pos2::new(
                    e.event_x as f32,
                    e.event_y as f32,
                )));
            }
            X::ConfigureNotify(e) => {
                if e.width > 0 && e.height > 0 && (e.width != self.width || e.height != self.height)
                {
                    self.width = e.width;
                    self.height = e.height;
                    self.pixels = vec![[0u8; 4]; e.width as usize * e.height as usize];
                }
            }
            X::ClientMessage(e) => {
                if e.type_ == self.wm_protocols && e.data.as_data32()[0] == self.wm_delete_window {
                    self.running = false;
                }
            }
            _ => {}
        }
        self.conn.flush()?;
        Ok(())
    }

    fn handle_key(&mut self, keycode: u8, state: u16, pressed: bool) -> Result<()> {
        let syms = self.keysyms_of(keycode);
        if syms.is_empty() {
            return Ok(());
        }
        let shift = state & 1 != 0;
        let level = if shift && syms.len() > 1 && syms[1] != 0 {
            1
        } else {
            0
        };
        let sym = syms[level.min(syms.len() - 1)];
        let modifiers = modifiers_from_state(state);

        if sym == KS_ESCAPE && pressed {
            self.running = false;
            return Ok(());
        }

        if let Some(key) = keysym_to_key(sym) {
            self.events.push(egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers,
            });
        }
        if pressed {
            // 与 winit 语义一致：Ctrl/Alt 按住时不发 Text 事件（那是快捷键组合）
            if !modifiers.ctrl && !modifiers.alt {
                if let Some(ch) = keysym_to_char(sym) {
                    self.events.push(egui::Event::Text(ch.to_string()));
                }
            }
        }
        Ok(())
    }

    fn handle_button(&mut self, state: u16, x: i16, y: i16, detail: u8, pressed: bool) {
        let pos = Pos2::new(x as f32, y as f32);
        let modifiers = modifiers_from_state(state);
        match detail {
            4 | 5 => {
                let y = if detail == 4 { 50.0 } else { -50.0 };
                self.events.push(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(0.0, y),
                    phase: egui::TouchPhase::Move,
                    modifiers,
                });
            }
            1..=3 => {
                let button = match detail {
                    1 => PointerButton::Primary,
                    2 => PointerButton::Middle,
                    _ => PointerButton::Secondary,
                };
                self.events.push(egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                });
            }
            _ => {}
        }
    }

    fn keysyms_of(&self, keycode: u8) -> &[u32] {
        let kpc = self.keysyms_per_keycode as usize;
        if kpc == 0 {
            return &[];
        }
        let idx = (keycode as usize - self.first_keycode as usize).saturating_mul(kpc);
        self.keymap.get(idx..idx + kpc).unwrap_or_default()
    }

    fn do_search(&mut self) {
        let word = self.input.trim().to_string();
        if word.is_empty() || self.state == State::Searching {
            return;
        }
        info!("Search requested for: {}", word);
        self.state = State::Searching;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(translation::translate(&word));
        });
    }

    #[allow(deprecated)] // egui 0.34 的 Panel::show 弃用但仍是根级面板的正确用法
    fn run_frame(
        &mut self,
        ctx: &egui::Context,
        renderer: &mut egui_software_backend::EguiSoftwareRender,
        start: &Instant,
    ) -> Result<()> {
        let w = self.width as usize;
        let h = self.height as usize;
        if self.pixels.len() != w * h {
            self.pixels = vec![[0u8; 4]; w * h];
        }

        let first = self.first_frame;
        self.first_frame = false;

        let raw = RawInput {
            viewport_id: ViewportId::ROOT,
            viewports: [(
                ViewportId::ROOT,
                ViewportInfo {
                    native_pixels_per_point: Some(1.0),
                    focused: Some(true),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            screen_rect: Some(egui::Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(w as f32, h as f32),
            )),
            focused: true,
            time: Some(start.elapsed().as_secs_f64()),
            events: std::mem::take(&mut self.events),
            ..Default::default()
        };
        ctx.begin_pass(raw);

        egui::TopBottomPanel::top("input_panel")
            .frame(egui::Frame::default().inner_margin(12.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.input)
                            .id(egui::Id::new("search_input"))
                            .font(egui::FontId::new(
                                INPUT_FONT_SIZE,
                                egui::FontFamily::Proportional,
                            ))
                            .vertical_align(egui::Align::Center)
                            .margin(TEXT_PADDING)
                            .desired_width(f32::INFINITY),
                    );
                    // 占位符：egui 的 hint_text 被硬编码为左上对齐，用 painter 自绘并垂直居中
                    if self.input.is_empty() {
                        ui.painter().text(
                            resp.rect.left_center() + egui::vec2(TEXT_PADDING, 0.0),
                            egui::Align2::LEFT_CENTER,
                            "输入单词，回车查询",
                            egui::FontId::new(
                                INPUT_FONT_SIZE * 0.8,
                                egui::FontFamily::Proportional,
                            ),
                            ui.visuals().weak_text_color(),
                        );
                    }
                    if first {
                        resp.request_focus();
                    }
                    let enter = ui.input(|i| i.key_pressed(Key::Enter));
                    if resp.lost_focus() && enter {
                        self.do_search();
                        resp.request_focus();
                    }
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(egui::Color32::from_rgb(24, 26, 30))
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| match &self.state {
                State::Searching => draw_searching(ui),
                State::NotFound => draw_not_found(ui),
                State::Idle => draw_empty_state(ui),
                State::Result(t) => draw_result(ui, t),
            });

        let full = ctx.end_pass();
        let primitives = ctx.tessellate(full.shapes, full.pixels_per_point);

        {
            let mut buffer = egui_software_backend::BufferMutRef::new(&mut self.pixels, w, h);
            renderer.render(
                &mut buffer,
                &primitives,
                &full.textures_delta,
                full.pixels_per_point,
            );
        }

        let bytes: &[u8] = bytemuck::cast_slice(&self.pixels);
        self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            self.window,
            self.gc,
            self.width,
            self.height,
            0,
            0,
            0,
            self.depth,
            bytes,
        )?;
        Ok(())
    }
}

/// XIM 输入法客户端状态（fcitx5）。
/// 回调产物（commit 文本 / 回传按键）暂存在队列里，由事件循环取出处理。
struct Ime {
    window: Window,
    im_id: u16,
    ic_id: u16,
    ready: bool,
    commits: Vec<String>,
    forwarded: Vec<KeyPressEvent>,
}

impl Ime {
    fn new(window: Window) -> Self {
        Self {
            window,
            im_id: 0,
            ic_id: 0,
            ready: false,
            commits: Vec::new(),
            forwarded: Vec::new(),
        }
    }
}

impl ClientHandler<X11rbClient<&RustConnection>> for Ime {
    fn handle_connect(
        &mut self,
        client: &mut X11rbClient<&RustConnection>,
    ) -> Result<(), ClientError> {
        info!("XIM 已连接");
        let locale = std::env::var("LANG").unwrap_or_else(|_| "zh_CN.UTF-8".into());
        client.open(&locale)
    }

    fn handle_open(
        &mut self,
        client: &mut X11rbClient<&RustConnection>,
        input_method_id: u16,
    ) -> Result<(), ClientError> {
        info!("XIM 输入法已打开，创建输入上下文");
        self.im_id = input_method_id;
        // Root 风格（PreeditNothing|StatusNothing）：fcitx 自己弹候选窗，客户端只收 commit
        let ic_attributes = client
            .build_ic_attributes()
            .push(
                AttributeName::InputStyle,
                InputStyle::PREEDIT_NOTHING | InputStyle::STATUS_NOTHING,
            )
            .push(AttributeName::ClientWindow, self.window)
            .push(AttributeName::FocusWindow, self.window)
            .nested_list(AttributeName::PreeditAttributes, |b| {
                b.push(AttributeName::SpotLocation, Point { x: 8, y: 20 });
            })
            .build();
        client.create_ic(input_method_id, ic_attributes)
    }

    fn handle_create_ic(
        &mut self,
        client: &mut X11rbClient<&RustConnection>,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        info!("输入上下文已创建（{input_context_id}），中文输入可用");
        self.ic_id = input_context_id;
        self.ready = true;
        client.set_focus(input_method_id, input_context_id)
    }

    fn handle_commit(
        &mut self,
        _client: &mut X11rbClient<&RustConnection>,
        _input_method_id: u16,
        _input_context_id: u16,
        text: &str,
    ) -> Result<(), ClientError> {
        self.commits.push(text.to_string());
        Ok(())
    }

    /// fcitx 未消费的按键会回传：交给正常输入处理
    fn handle_forward_event(
        &mut self,
        _client: &mut X11rbClient<&RustConnection>,
        _input_method_id: u16,
        _input_context_id: u16,
        _flag: ForwardEventFlag,
        xev: KeyPressEvent,
    ) -> Result<(), ClientError> {
        self.forwarded.push(xev);
        Ok(())
    }

    fn handle_disconnect(&mut self) {
        self.ready = false;
        warn!("XIM 断开连接");
    }

    fn handle_close(
        &mut self,
        client: &mut X11rbClient<&RustConnection>,
        _input_method_id: u16,
    ) -> Result<(), ClientError> {
        self.ready = false;
        client.disconnect()
    }

    fn handle_destroy_ic(
        &mut self,
        client: &mut X11rbClient<&RustConnection>,
        input_method_id: u16,
        _input_context_id: u16,
    ) -> Result<(), ClientError> {
        client.close(input_method_id)
    }
}

fn modifiers_from_state(state: u16) -> Modifiers {
    let ctrl = state & (1 << 2) != 0; // CONTROL
    Modifiers {
        alt: state & (1 << 3) != 0, // MOD1
        ctrl,
        shift: state & (1 << 0) != 0,
        mac_cmd: false,
        command: ctrl,
    }
}

fn keysym_to_key(sym: u32) -> Option<Key> {
    if let Some(key) = match sym {
        KS_BACKSPACE => Some(Key::Backspace),
        KS_TAB => Some(Key::Tab),
        KS_RETURN => Some(Key::Enter),
        KS_HOME => Some(Key::Home),
        KS_LEFT => Some(Key::ArrowLeft),
        KS_UP => Some(Key::ArrowUp),
        KS_RIGHT => Some(Key::ArrowRight),
        KS_DOWN => Some(Key::ArrowDown),
        KS_PAGE_UP => Some(Key::PageUp),
        KS_PAGE_DOWN => Some(Key::PageDown),
        KS_END => Some(Key::End),
        KS_DELETE => Some(Key::Delete),
        _ => None,
    } {
        return Some(key);
    }
    // ASCII 字母/数字/空格：映射成 egui Key，使 Ctrl+A 等快捷键可用
    if let Some(ch) = keysym_to_char(sym) {
        if ch.is_ascii_alphanumeric() || ch == ' ' {
            return Key::from_name(&ch.to_string());
        }
    }
    None
}

fn keysym_to_char(sym: u32) -> Option<char> {
    if (0x20..=0x7E).contains(&sym) {
        return char::from_u32(sym);
    }
    // Unicode keysym（U+01000000 起）
    if sym >= 0x0100_0000 {
        return char::from_u32(sym - 0x0100_0000);
    }
    None
}

/// 搜索中状态：占面板一半空间的居中指示块。
fn draw_searching(ui: &mut egui::Ui) {
    let avail = ui.available_rect_before_wrap();
    let half = egui::Rect::from_center_size(
        avail.center(),
        egui::vec2(avail.width() * 0.5, avail.height() * 0.5),
    );

    ui.painter()
        .rect_filled(half, 12.0, egui::Color32::from_rgb(34, 37, 43));
    ui.painter().rect_stroke(
        half,
        12.0,
        egui::Stroke::new(1.0_f32, egui::Color32::from_gray(70)),
        egui::StrokeKind::Inside,
    );

    ui.scope_builder(egui::UiBuilder::new().max_rect(half), |ui| {
        ui.vertical_centered(|ui| {
            let content_h = 44.0 + 12.0 + 28.0;
            ui.add_space(((half.height() - content_h) * 0.5).max(0.0));
            ui.add(egui::Spinner::new().size(44.0));
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new("正在搜索…")
                    .size(20.0)
                    .color(egui::Color32::from_gray(200)),
            );
        });
    });
}

/// 空状态：居中的放大镜图形（表示"还没有任何内容"）。
fn draw_empty_state(ui: &mut egui::Ui) {
    let center = ui.available_rect_before_wrap().center();
    draw_magnifier(ui, center, egui::Color32::from_gray(85));
    ui.painter().text(
        center + egui::vec2(0.0, 95.0),
        egui::Align2::CENTER_CENTER,
        "输入单词，回车查询",
        egui::FontId::new(18.0, egui::FontFamily::Proportional),
        egui::Color32::from_gray(150),
    );
}

/// 未找到状态：放大镜 + X 标记，样式与启动空状态一致。
fn draw_not_found(ui: &mut egui::Ui) {
    let center = ui.available_rect_before_wrap().center();
    let lens = center - egui::vec2(0.0, 8.0);
    let radius = 46.0;

    draw_magnifier(ui, center, egui::Color32::from_gray(85));

    // 镜片内的 X 标记（淡红）
    let x_stroke = egui::Stroke::new(5.0_f32, egui::Color32::from_rgb(205, 100, 100));
    let half = radius * 0.38;
    ui.painter().line_segment(
        [
            lens + egui::vec2(-half, -half),
            lens + egui::vec2(half, half),
        ],
        x_stroke,
    );
    ui.painter().line_segment(
        [
            lens + egui::vec2(-half, half),
            lens + egui::vec2(half, -half),
        ],
        x_stroke,
    );

    ui.painter().text(
        center + egui::vec2(0.0, 95.0),
        egui::Align2::CENTER_CENTER,
        "未找到该单词，换个拼写试试",
        egui::FontId::new(18.0, egui::FontFamily::Proportional),
        egui::Color32::from_gray(150),
    );
}

/// 结果状态：逐行渲染（圆点 + 文字），行距 +8px。
fn draw_result(ui: &mut egui::Ui, text: &str) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y += 8.0;
            let text_color = egui::Color32::from_rgb(225, 230, 235);
            let dot_color = egui::Color32::from_rgb(170, 180, 190);
            // 每行左侧的圆点：自绘，直径为字体大小的 1/3
            let dot_diameter = TRANSLATION_FONT_SIZE / 3.0;
            for line in text.lines() {
                if line.is_empty() {
                    continue;
                }
                ui.horizontal(|ui| {
                    if let Some(rest) = line.strip_prefix("· ") {
                        // 先占位（锁定 x 位置），label 加入后再绘制圆点，
                        // y 对齐到文字行中心 —— 水平布局按初始行高居中会偏上
                        let (dot_rect, _) = ui.allocate_exact_size(
                            egui::vec2(dot_diameter, dot_diameter),
                            egui::Sense::hover(),
                        );
                        // 音标行（API 格式 · [..]）整体用 DejaVu Sans
                        let font_id = if rest.starts_with('[') {
                            egui::FontId::new(
                                TRANSLATION_FONT_SIZE,
                                egui::FontFamily::Name("ipa".into()),
                            )
                        } else {
                            egui::FontId::new(TRANSLATION_FONT_SIZE, egui::FontFamily::Proportional)
                        };
                        let label_resp = ui.add(
                            egui::Label::new(RichText::new(rest).font(font_id).color(text_color))
                                .wrap(),
                        );
                        let center = egui::pos2(dot_rect.center().x, label_resp.rect.center().y);
                        ui.painter()
                            .circle_filled(center, dot_diameter / 2.0, dot_color);
                    } else {
                        ui.add(
                            egui::Label::new(
                                RichText::new(line)
                                    .size(TRANSLATION_FONT_SIZE)
                                    .color(text_color),
                            )
                            .wrap(),
                        );
                    }
                });
            }
        });
}

/// 放大镜图形：镜片圆环 + 手柄。
fn draw_magnifier(ui: &mut egui::Ui, center: egui::Pos2, color: egui::Color32) {
    let stroke = egui::Stroke::new(5.0_f32, color);
    let lens = center - egui::vec2(0.0, 8.0);
    let radius = 46.0;
    ui.painter().circle_stroke(lens, radius, stroke);
    let d = radius * std::f32::consts::FRAC_1_SQRT_2;
    let handle = egui::vec2(d, d);
    ui.painter()
        .line_segment([lens + handle, lens + handle * 1.6], stroke);
}
