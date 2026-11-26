#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

use eframe::egui;
use gemini_proxy::{
    config::AppConfig,
    local_browser_url, open_in_browser, run_server_with_state,
    terminal::{
        manager::{CreateSessionParams, SessionView, TerminalManager},
        session::{AgentKind, capture_session_plain, send_session_paste},
    },
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainTab {
    Terminal,
    ApiTest,
}

struct ApiTestResult {
    status: String,
    headers_summary: String,
    body: String,
}

struct GeminiProxyApp {
    config: Arc<AppConfig>,
    manager: Arc<TerminalManager>,
    listen_addr: SocketAddr,
    sessions: Vec<SessionView>,
    selected_session_id: Option<String>,
    last_refresh: Instant,
    terminal_snapshot: String,
    terminal_input: String,
    active_tab: MainTab,
    // New session form
    new_name: String,
    new_model: String,
    new_cwd: String,
    new_cmd: String,
    new_agent_kind: AgentKind,
    status_banner: String,
    // API Test tab
    test_model: String,
    test_prompt: String,
    test_in_flight: bool,
    test_output: String,
    test_rx: Receiver<ApiTestResult>,
    test_tx: Sender<ApiTestResult>,
}

impl GeminiProxyApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        config: Arc<AppConfig>,
        manager: Arc<TerminalManager>,
        listen_addr: SocketAddr,
    ) -> Self {
        install_optional_cjk_font(&cc.egui_ctx);
        let sessions = manager.list_session_views();
        let selected_session_id = sessions.first().map(|s| s.id.clone());
        let default_model = config
            .list_models()
            .first()
            .map(|m| m.id.clone())
            .or_else(|| sessions.first().map(|s| s.model_alias.clone()))
            .unwrap_or_else(|| "gemini-proxy-agent".to_string());
        let (test_tx, test_rx) = mpsc::channel();
        let default_cwd = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .display()
            .to_string();

        let mut app = Self {
            config,
            manager,
            listen_addr,
            sessions,
            selected_session_id,
            last_refresh: Instant::now() - Duration::from_secs(5),
            terminal_snapshot: String::new(),
            terminal_input: String::new(),
            active_tab: MainTab::Terminal,
            new_name: String::new(),
            new_model: String::new(),
            new_cwd: default_cwd,
            new_cmd: String::new(),
            new_agent_kind: AgentKind::Auto,
            status_banner: format!("Ready on {}", local_browser_url(listen_addr)),
            test_model: default_model,
            test_prompt: "请只回复 OK_GUI_TEST".to_string(),
            test_in_flight: false,
            test_output: "等待测试...".to_string(),
            test_rx,
            test_tx,
        };
        app.refresh_state();
        app
    }

    fn refresh_state(&mut self) {
        self.sessions = self.manager.list_session_views();
        if let Some(ref sel) = self.selected_session_id {
            if !self.sessions.iter().any(|s| &s.id == sel) {
                self.selected_session_id = self.sessions.first().map(|s| s.id.clone());
            }
        } else {
            self.selected_session_id = self.sessions.first().map(|s| s.id.clone());
        }

        if let Some(ref sel) = self.selected_session_id {
            if let Some(sess) = self.manager.get_session(sel) {
                if let Ok(text) = capture_session_plain(&sess, 160) {
                    self.terminal_snapshot = text;
                }
            }
        } else {
            self.terminal_snapshot.clear();
        }
        self.last_refresh = Instant::now();
    }

    fn start_api_test(&mut self) {
        let Some(api_key) = self.config.primary_api_key_secret() else {
            self.test_output = "未配置 API Key".to_string();
            return;
        };
        let model = self.test_model.trim().to_string();
        let prompt = self.test_prompt.trim().to_string();
        if model.is_empty() || prompt.is_empty() {
            self.test_output = "Model 和 Prompt 不能为空".to_string();
            return;
        }
        self.test_in_flight = true;
        self.test_output = "正在通过 /v1/chat/completions 调用终端 Agent...".to_string();
        let tx = self.test_tx.clone();
        let port = self.listen_addr.port();

        std::thread::spawn(move || {
            let res = run_loopback_chat_completion(port, &api_key, &model, &prompt);
            let _ = tx.send(res);
        });
    }
}

impl eframe::App for GeminiProxyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(res) = self.test_rx.try_recv() {
            self.test_in_flight = false;
            self.test_output = format!(
                "{}\n{}\n\n{}",
                res.status, res.headers_summary, res.body
            );
            self.refresh_state();
        }

        if self.last_refresh.elapsed() >= Duration::from_millis(600) {
            self.refresh_state();
        }
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("geminiProxy");
                ui.separator();
                let url = local_browser_url(self.listen_addr);
                ui.label(format!("API/Web: {url}"));
                ui.label(format!("Key: {}", self.config.primary_api_key_masked()));
                if ui.button("复制 API Key").clicked() {
                    if let Some(key) = self.config.primary_api_key_secret() {
                        ctx.copy_text(key);
                        self.status_banner = "已复制 API Key 到剪贴板".to_string();
                    }
                }
                if ui.button("在浏览器打开 Web 终端").clicked() {
                    if let Err(err) = open_in_browser(&url) {
                        self.status_banner = format!("打开浏览器失败: {err}");
                    } else {
                        self.status_banner = format!("已打开 {url}");
                    }
                }
                if ui.button("刷新").clicked() {
                    self.refresh_state();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(&self.status_banner);
                });
            });
        });

        egui::SidePanel::left("sidebar")
            .default_width(310.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.heading(format!("终端会话 ({})", self.sessions.len()));
                ui.add_space(4.0);

                egui::ScrollArea::vertical()
                    .id_salt("sessions_scroll")
                    .max_height(260.0)
                    .show(ui, |ui| {
                        if self.sessions.is_empty() {
                            ui.weak("暂无运行中会话（请求模型时将自动拉起，或下方手动新建）");
                        }
                        let mut to_delete = None;
                        for s in &self.sessions {
                            let is_selected =
                                self.selected_session_id.as_deref() == Some(s.id.as_str());
                            let state_tag = if !s.alive {
                                "dead"
                            } else if s.busy {
                                "busy"
                            } else {
                                "idle"
                            };
                            let model_label = s
                                .detected_model
                                .as_deref()
                                .map(|m| format!(" · active={m}"))
                                .unwrap_or_default();
                            let title = format!(
                                "{} [{}]\nmodel={} · {}{} · turns={}",
                                s.name,
                                state_tag,
                                s.model_alias,
                                s.detected_agent,
                                model_label,
                                s.turn_count
                            );
                            ui.horizontal(|ui| {
                                if ui.selectable_label(is_selected, title).clicked() {
                                    self.selected_session_id = Some(s.id.clone());
                                    self.last_refresh = Instant::now() - Duration::from_secs(5);
                                }
                                if ui.small_button("销毁").clicked() {
                                    to_delete = Some(s.id.clone());
                                }
                            });
                            ui.separator();
                        }
                        if let Some(id) = to_delete {
                            match self.manager.delete_session(&id) {
                                Ok(_) => {
                                    self.status_banner = format!("已销毁会话 {id}");
                                    self.refresh_state();
                                }
                                Err(err) => {
                                    self.status_banner = format!("销毁失败: {err}");
                                }
                            }
                        }
                    });

                ui.add_space(6.0);
                egui::CollapsingHeader::new("+ 新建终端会话")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("名称:");
                            ui.text_edit_singleline(&mut self.new_name);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Model:");
                            ui.text_edit_singleline(&mut self.new_model);
                        });
                        ui.horizontal(|ui| {
                            ui.label("目录:");
                            ui.text_edit_singleline(&mut self.new_cwd);
                        });
                        ui.horizontal(|ui| {
                            ui.label("启动命令:");
                            ui.text_edit_singleline(&mut self.new_cmd);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Agent:");
                            egui::ComboBox::from_id_salt("new_agent_kind")
                                .selected_text(format!("{:?}", self.new_agent_kind))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.new_agent_kind,
                                        AgentKind::Auto,
                                        "Auto",
                                    );
                                    ui.selectable_value(
                                        &mut self.new_agent_kind,
                                        AgentKind::CodeBuddy,
                                        "CodeBuddy",
                                    );
                                    ui.selectable_value(
                                        &mut self.new_agent_kind,
                                        AgentKind::Agy,
                                        "Agy",
                                    );
                                    ui.selectable_value(
                                        &mut self.new_agent_kind,
                                        AgentKind::Codex,
                                        "Codex",
                                    );
                                    ui.selectable_value(
                                        &mut self.new_agent_kind,
                                        AgentKind::Claude,
                                        "Claude",
                                    );
                                    ui.selectable_value(
                                        &mut self.new_agent_kind,
                                        AgentKind::Generic,
                                        "Generic",
                                    );
                                });
                        });
                        if ui.button("创建并连接").clicked() {
                            let name = self.new_name.trim().to_string();
                            if name.is_empty() {
                                self.status_banner = "请输入会话名称".to_string();
                            } else {
                                let params = CreateSessionParams {
                                    name,
                                    model_alias: (!self.new_model.trim().is_empty())
                                        .then(|| self.new_model.trim().to_string()),
                                    cwd: (!self.new_cwd.trim().is_empty())
                                        .then(|| PathBuf::from(self.new_cwd.trim())),
                                    startup_command: (!self.new_cmd.trim().is_empty())
                                        .then(|| self.new_cmd.trim().to_string()),
                                    agent_kind: self.new_agent_kind,
                                    use_markers: false,
                                    owner_key_id: None,
                                    managed: false,
                                    explicit_conversation_id: None,
                                };
                                match self.manager.create_session(params) {
                                    Ok(sess) => {
                                        self.selected_session_id = Some(sess.id.clone());
                                        self.status_banner = format!("已创建会话 {}", sess.id);
                                        self.new_name.clear();
                                        self.refresh_state();
                                    }
                                    Err(err) => {
                                        self.status_banner = format!("创建失败: {err:#}");
                                    }
                                }
                            }
                        }
                    });

                ui.add_space(6.0);
                egui::CollapsingHeader::new("模型目录 (自动拉起)").show(ui, |ui| {
                    for m in self.config.list_models() {
                        ui.label(format!("• {} → {}", m.id, m.launch_command));
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.active_tab, MainTab::Terminal, "实时终端");
                ui.selectable_value(&mut self.active_tab, MainTab::ApiTest, "API 代理测试");
            });
            ui.separator();

            match self.active_tab {
                MainTab::Terminal => {
                    if let Some(ref sel) = self.selected_session_id.clone() {
                        ui.label(format!("当前会话: {sel}"));
                        let avail_height = (ui.available_height() - 42.0).max(160.0);
                        egui::ScrollArea::vertical()
                            .id_salt("term_pane_scroll")
                            .stick_to_bottom(true)
                            .max_height(avail_height)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut self.terminal_snapshot.as_str())
                                        .font(egui::TextStyle::Monospace)
                                        .desired_width(f32::INFINITY),
                                );
                            });
                        ui.horizontal(|ui| {
                            let enter_pressed = ui
                                .add(
                                    egui::TextEdit::singleline(&mut self.terminal_input)
                                        .desired_width(ui.available_width() - 160.0)
                                        .hint_text("输入命令或对话内容，回车发送..."),
                                )
                                .lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if enter_pressed || ui.button("发送").clicked() {
                                if let Some(sess) = self.manager.get_session(sel) {
                                    let text = std::mem::take(&mut self.terminal_input);
                                    match send_session_paste(&sess, &text, true) {
                                        Ok(()) => self.refresh_state(),
                                        Err(err) => {
                                            self.status_banner = format!("发送失败: {err}");
                                        }
                                    }
                                }
                            }
                            if ui.button("Enter").clicked() {
                                if let Some(sess) = self.manager.get_session(sel) {
                                    let _ = send_session_paste(&sess, "", true);
                                    self.refresh_state();
                                }
                            }
                        });
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.label("请先在左侧选择或创建一个终端会话");
                        });
                    }
                }
                MainTab::ApiTest => {
                    ui.horizontal(|ui| {
                        ui.label("Model:");
                        ui.text_edit_singleline(&mut self.test_model);
                    });
                    ui.label("Prompt:");
                    ui.add(
                        egui::TextEdit::multiline(&mut self.test_prompt)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_enabled_ui(!self.test_in_flight, |ui| {
                        if ui.button("发送请求 (/v1/chat/completions)").clicked() {
                            self.start_api_test();
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("api_test_scroll")
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut self.test_output.as_str())
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY),
                            );
                        });
                }
            }
        });
    }
}

fn run_loopback_chat_completion(
    port: u16,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> ApiTestResult {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("valid loopback");
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
    })
    .to_string();

    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Ok(s) => s,
        Err(err) => {
            return ApiTestResult {
                status: "连接失败".to_string(),
                headers_summary: String::new(),
                body: err.to_string(),
            };
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {api_key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if let Err(err) = stream.write_all(req.as_bytes()).and_then(|_| stream.flush()) {
        return ApiTestResult {
            status: "发送失败".to_string(),
            headers_summary: String::new(),
            body: err.to_string(),
        };
    }

    let mut raw = Vec::new();
    if let Err(err) = stream.read_to_end(&mut raw) {
        return ApiTestResult {
            status: "读取失败".to_string(),
            headers_summary: String::new(),
            body: err.to_string(),
        };
    }
    let text = String::from_utf8_lossy(&raw);
    let Some((head, resp_body)) = text.split_once("\r\n\r\n") else {
        return ApiTestResult {
            status: "无效响应".to_string(),
            headers_summary: String::new(),
            body: text.to_string(),
        };
    };
    let status_line = head.lines().next().unwrap_or("").to_string();
    let interesting_headers: Vec<&str> = head
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("x-gemini-proxy"))
        .collect();
    let pretty_body = serde_json::from_str::<serde_json::Value>(resp_body)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| resp_body.to_string());

    ApiTestResult {
        status: status_line,
        headers_summary: interesting_headers.join(" | "),
        body: pretty_body,
    }
}

fn install_optional_cjk_font(ctx: &egui::Context) {
    let candidates = [
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/wqy-zenhei/wqy-zenhei.ttc",
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".to_string(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk".to_string());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_string());
            ctx.set_fonts(fonts);
            break;
        }
    }
}

fn main() -> eframe::Result<()> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(env_filter).try_init();

    let config = Arc::new(AppConfig::load().expect("failed to load geminiProxy config"));
    let manager = TerminalManager::new(&config.data_dir).expect("failed to init TerminalManager");

    let (addr_tx, addr_rx) = mpsc::channel::<Result<SocketAddr, String>>();
    let server_config = config.clone();
    let server_manager = manager.clone();

    std::thread::Builder::new()
        .name("gemini-proxy-server".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
                let fwd_tx = addr_tx.clone();
                tokio::spawn(async move {
                    if let Ok(addr) = bound_rx.await {
                        let _ = fwd_tx.send(Ok(addr));
                    }
                });
                if let Err(err) = run_server_with_state(
                    server_config,
                    server_manager,
                    std::future::pending::<()>(),
                    Some(bound_tx),
                )
                .await
                {
                    let _ = addr_tx.send(Err(err.to_string()));
                }
            });
        })
        .expect("failed to spawn server thread");

    let listen_addr = match addr_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server bind timeout")
    {
        Ok(addr) => addr,
        Err(err) => panic!("failed to start embedded server: {err}"),
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1180.0, 760.0]),
        ..Default::default()
    };

    eframe::run_native(
        "geminiProxy — Desktop & Web Terminal LLM Proxy",
        options,
        Box::new(move |cc| {
            Ok(Box::new(GeminiProxyApp::new(
                cc,
                config,
                manager,
                listen_addr,
            )))
        }),
    )
}
