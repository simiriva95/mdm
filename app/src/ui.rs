//! UI stile "desktop di terminali": sfondo chiaro, finestre navy con chrome
//! colorato [X][+][_], gauge pastello con mappa segmenti, sparkline di rete,
//! tab Downloads/Console e topolino pixel che corre in basso.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, pos2, vec2, Align, Color32, FontId, Layout, Rect, RichText, Rounding, Sense, Stroke,
    TextStyle,
};

use crate::engine::{self, AppState, Download, Status};

// palette dal riferimento: desktop periwinkle, pannelli navy, accenti pastello
const DESK: Color32 = Color32::from_rgb(0xb4, 0xbf, 0xd8);
const DESK_TAG: Color32 = Color32::from_rgb(0xe8, 0xec, 0xf6);
const DESK_TAG_OFF: Color32 = Color32::from_rgb(0xc4, 0xcd, 0xe2);
const DESK_TEXT: Color32 = Color32::from_rgb(0x2c, 0x31, 0x4a);
const PANEL: Color32 = Color32::from_rgb(0x2a, 0x2e, 0x48);
const PANEL_DARK: Color32 = Color32::from_rgb(0x23, 0x26, 0x3c);
const TRACK: Color32 = Color32::from_rgb(0x1a, 0x1c, 0x2e);
const CHROME: Color32 = Color32::from_rgb(0x45, 0x4b, 0x70);
const TEXT: Color32 = Color32::from_rgb(0xd9, 0xdd, 0xec);
const MUTED: Color32 = Color32::from_rgb(0x8d, 0x94, 0xb8);
const MINT: Color32 = Color32::from_rgb(0x9d, 0xe3, 0xc2);
const PINK: Color32 = Color32::from_rgb(0xe8, 0xa0, 0xa8);
const AMBER: Color32 = Color32::from_rgb(0xe9, 0xc8, 0x85);
const BLUE: Color32 = Color32::from_rgb(0x88, 0xaa, 0xe4);
const RED: Color32 = Color32::from_rgb(0xe8, 0x7c, 0x7c);
const PET_BODY: Color32 = Color32::from_rgb(0x6e, 0x76, 0x9c);

pub fn run(state: Arc<AppState>) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([560.0, 400.0])
            .with_title("MDM"),
        ..Default::default()
    };
    eframe::run_native(
        "MDM",
        options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            *state.egui_ctx.lock().unwrap() = Some(cc.egui_ctx.clone());
            state.log("MDM v0.2 pronto — in ascolto su 127.0.0.1:48666");
            Ok(Box::new(App::new(state, &cc.egui_ctx)))
        }),
    )
}

fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = DESK;
    style.visuals.window_fill = PANEL;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, CHROME);
    style.visuals.widgets.inactive.bg_fill = PANEL_DARK;
    style.visuals.widgets.inactive.weak_bg_fill = PANEL_DARK;
    style.visuals.widgets.hovered.bg_fill = CHROME;
    style.visuals.widgets.hovered.weak_bg_fill = CHROME;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, MUTED);
    style.visuals.widgets.active.bg_fill = CHROME;
    style.visuals.widgets.active.weak_bg_fill = CHROME;
    style.visuals.selection.bg_fill = CHROME;
    for (text_style, size) in [
        (TextStyle::Heading, 15.0),
        (TextStyle::Body, 13.0),
        (TextStyle::Button, 12.0),
        (TextStyle::Small, 11.0),
        (TextStyle::Monospace, 13.0),
    ] {
        style.text_styles.insert(text_style, FontId::monospace(size));
    }
    style.spacing.item_spacing = vec2(8.0, 6.0);
    style.spacing.button_padding = vec2(8.0, 3.0);
    ctx.set_style(style);
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Downloads,
    Console,
}

struct SpeedSample {
    at: Instant,
    bytes: u64,
    speed: f64, // byte/s
}

struct App {
    state: Arc<AppState>,
    tab: Tab,
    speeds: HashMap<u64, SpeedSample>,
    net_history: VecDeque<f32>,
    net_peak: f64,
    last_net_sample: Instant,
    #[cfg(windows)]
    tray: Option<Tray>,
    #[cfg(windows)]
    quitting: bool,
}

impl App {
    fn new(state: Arc<AppState>, _ctx: &egui::Context) -> Self {
        Self {
            state,
            tab: Tab::Downloads,
            speeds: HashMap::new(),
            net_history: VecDeque::with_capacity(160),
            net_peak: 0.0,
            last_net_sample: Instant::now(),
            #[cfg(windows)]
            tray: Tray::build(_ctx),
            #[cfg(windows)]
            quitting: false,
        }
    }

    fn speed_of(&mut self, id: u64, done: u64) -> f64 {
        let now = Instant::now();
        let s = self.speeds.entry(id).or_insert(SpeedSample { at: now, bytes: done, speed: 0.0 });
        let dt = now.duration_since(s.at).as_secs_f64();
        if dt >= 0.5 {
            s.speed = (done.saturating_sub(s.bytes)) as f64 / dt;
            s.at = now;
            s.bytes = done;
        }
        s.speed
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.state.show_window.swap(false, Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        #[cfg(windows)]
        if let Some(tray) = &self.tray {
            match tray.poll() {
                TrayAction::Open => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayAction::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                TrayAction::None => {}
            }
        }

        #[cfg(windows)]
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        let downloads = self.state.downloads.lock().unwrap().clone();
        let n_active = downloads
            .iter()
            .filter(|d| matches!(*d.status.lock().unwrap(), Status::Active | Status::Connecting))
            .count();

        // velocità totale per sparkline e picco
        let total_speed: f64 = downloads
            .iter()
            .map(|d| {
                let done = d.done.load(Ordering::Relaxed);
                self.speed_of(d.id, done)
            })
            .sum();
        if self.last_net_sample.elapsed() >= Duration::from_millis(500) {
            self.last_net_sample = Instant::now();
            self.net_history.push_back(total_speed as f32);
            if self.net_history.len() > 150 {
                self.net_history.pop_front();
            }
            self.net_peak = self.net_peak.max(total_speed);
        }

        // barra "desktop" in alto: tag + orologio
        egui::TopBottomPanel::top("desk_top").frame(egui::Frame::none().fill(DESK)).show(ctx, |ui| {
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.add_space(2.0);
                desk_tag(ui, "[ MDM — download manager ]", true);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(2.0);
                    ui.label(RichText::new(chrono::Local::now().format("%H:%M:%S").to_string()).color(DESK_TEXT).strong());
                });
            });
            ui.add_space(5.0);
        });

        // in fondo: striscia dove corre il topolino
        egui::TopBottomPanel::bottom("pet_strip").frame(egui::Frame::none().fill(DESK)).show(ctx, |ui| {
            pet_strip(ui, ctx.input(|i| i.time));
        });

        // sopra il topolino: tab cliccabili
        egui::TopBottomPanel::bottom("desk_tabs").frame(egui::Frame::none().fill(DESK)).show(ctx, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.add_space(2.0);
                if desk_tag(ui, "[ Downloads ]", self.tab == Tab::Downloads) {
                    self.tab = Tab::Downloads;
                }
                if desk_tag(ui, "[ Console ]", self.tab == Tab::Console) {
                    self.tab = Tab::Console;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(2.0);
                    let (dot, txt) = if n_active > 0 {
                        (AMBER, format!("● {n_active} ACTIVE"))
                    } else {
                        (DESK_TEXT, "● IDLE".to_string())
                    };
                    ui.label(RichText::new(format!("port 48666  ")).color(DESK_TEXT));
                    ui.label(RichText::new(txt).color(dot));
                });
            });
            ui.add_space(3.0);
        });

        egui::CentralPanel::default().frame(egui::Frame::none().fill(DESK).inner_margin(6.0)).show(ctx, |ui| {
            match self.tab {
                Tab::Console => self.console_tab(ui),
                Tab::Downloads => self.downloads_tab(ui, &downloads, n_active, total_speed),
            }
        });

        // 15fps: bastano per topolino, orologio e progressi
        ctx.request_repaint_after(Duration::from_millis(66));
    }
}

impl App {
    fn downloads_tab(&mut self, ui: &mut egui::Ui, downloads: &[Arc<Download>], n_active: usize, total_speed: f64) {
        let net_h = 118.0;
        let top_h = (ui.available_height() - net_h - 6.0).max(140.0);

        let title = if n_active > 0 { format!("Downloads — {n_active} attivi") } else { "Downloads".to_string() };
        let mut to_remove: Vec<u64> = Vec::new();
        ui.allocate_ui(vec2(ui.available_width(), top_h), |ui| {
            term_window(ui, &title, PINK, |ui| {
                ui.set_min_height(top_h - 50.0);
                if downloads.is_empty() {
                    ui.add_space(28.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("┌──────────────────────────────────┐").color(CHROME));
                        ui.label(RichText::new("│  in attesa di download da Chrome  │").color(MUTED));
                        ui.label(RichText::new("└──────────────────────────────────┘").color(CHROME));
                        ui.add_space(6.0);
                        ui.label(RichText::new("icona estensione = ON/OFF · >10MB passano di qui").color(CHROME).size(11.0));
                    });
                    return;
                }
                egui::ScrollArea::vertical().id_salt("dl_scroll").show(ui, |ui| {
                    for dl in downloads.iter().rev() {
                        self.download_row(ui, dl, &mut to_remove);
                    }
                });
            });
        });
        if !to_remove.is_empty() {
            self.state.downloads.lock().unwrap().retain(|d| !to_remove.contains(&d.id));
        }

        ui.add_space(2.0);

        // pannello rete a tutta larghezza
        term_window(ui, "net", MINT, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("▼ {}/s", fmt_bytes(total_speed as u64))).color(MINT).size(17.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let session_total: u64 = downloads.iter().map(|d| d.done.load(Ordering::Relaxed)).sum();
                    ui.label(
                        RichText::new(format!("picco {}/s · sessione {}", fmt_bytes(self.net_peak as u64), fmt_bytes(session_total)))
                            .color(MUTED)
                            .size(11.0),
                    );
                });
            });
            sparkline(ui, &self.net_history, 46.0);
        });
    }

    fn console_tab(&mut self, ui: &mut egui::Ui) {
        let h = ui.available_height();
        ui.allocate_ui(vec2(ui.available_width(), h), |ui| {
            term_window(ui, "console — log completo", BLUE, |ui| {
                ui.set_min_height(h - 50.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if tbtn(ui, "[ clear ]", MUTED) {
                            self.state.log.lock().unwrap().clear();
                        }
                    });
                });
                egui::ScrollArea::vertical().id_salt("log_full").stick_to_bottom(true).show(ui, |ui| {
                    for line in self.state.log.lock().unwrap().iter() {
                        let color = if line.contains("ERRORE") {
                            RED
                        } else if line.contains("completato") {
                            MINT
                        } else {
                            MUTED
                        };
                        ui.label(RichText::new(line).color(color).size(12.0));
                    }
                });
            });
        });
    }

    fn download_row(&mut self, ui: &mut egui::Ui, dl: &Arc<Download>, to_remove: &mut Vec<u64>) {
        let done = dl.done.load(Ordering::Relaxed);
        let total = dl.total.load(Ordering::Relaxed);
        let status = dl.status.lock().unwrap().clone();
        let name = dl.name.lock().unwrap().clone();
        let speed = self.speed_of(dl.id, done);

        egui::Frame::none()
            .fill(PANEL_DARK)
            .rounding(Rounding::same(4.0))
            .inner_margin(egui::Margin::same(8.0))
            .outer_margin(egui::Margin::symmetric(0.0, 3.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (dot, color) = match status {
                        Status::Active => ("▶", MINT),
                        Status::Connecting => ("…", AMBER),
                        Status::Paused => ("⏸", AMBER),
                        Status::Done => ("✔", MINT),
                        Status::Failed(_) => ("✗", RED),
                        Status::Cancelled => ("–", MUTED),
                    };
                    ui.label(RichText::new(dot).color(color));
                    ui.add(egui::Label::new(RichText::new(&name).color(TEXT).strong()).truncate());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        match &status {
                            Status::Active | Status::Connecting => {
                                if tbtn(ui, "[ abort ]", RED) {
                                    dl.cancel.store(true, Ordering::Relaxed);
                                }
                                if tbtn(ui, "[ pause ]", AMBER) {
                                    dl.pause.store(true, Ordering::Relaxed);
                                }
                            }
                            Status::Paused | Status::Failed(_) => {
                                if tbtn(ui, "[ x ]", MUTED) {
                                    engine::discard(dl);
                                    to_remove.push(dl.id);
                                }
                                if tbtn(ui, "[ resume ]", MINT) {
                                    let rt = self.state.rt.lock().unwrap().clone();
                                    if let Some(rt) = rt {
                                        rt.spawn(engine::resume(self.state.clone(), dl.clone()));
                                    }
                                }
                            }
                            Status::Done => {
                                if tbtn(ui, "[ x ]", MUTED) {
                                    to_remove.push(dl.id);
                                }
                                if tbtn(ui, "[ open ]", BLUE) {
                                    reveal(&dl.path.lock().unwrap());
                                }
                            }
                            Status::Cancelled => {
                                if tbtn(ui, "[ x ]", MUTED) {
                                    to_remove.push(dl.id);
                                }
                            }
                        }
                    });
                });

                ui.add_space(2.0);
                seg_gauge(ui, dl, &status, 12.0);
                ui.add_space(2.0);

                let pct = if total > 0 { format!("{:>3.0}%  ", done as f64 / total as f64 * 100.0) } else { String::new() };
                let info = match &status {
                    Status::Connecting => "connessione...".to_string(),
                    Status::Active => {
                        let conns = dl.conc.load(Ordering::Relaxed);
                        let eta = if speed > 1.0 && total > done {
                            fmt_eta((total - done) as f64 / speed)
                        } else {
                            "--:--".into()
                        };
                        if total > 0 {
                            format!(
                                "{pct}{} / {}   ▼ {}/s   eta {}   {} conn",
                                fmt_bytes(done), fmt_bytes(total), fmt_bytes(speed as u64), eta, conns
                            )
                        } else {
                            format!("{}   ▼ {}/s", fmt_bytes(done), fmt_bytes(speed as u64))
                        }
                    }
                    Status::Paused => format!("{pct}{} / {}   in pausa — riprende dal punto esatto", fmt_bytes(done), fmt_bytes(total)),
                    Status::Done => format!("{}   completato", fmt_bytes(done.max(total))),
                    Status::Failed(e) => format!("fallito: {e} — [ resume ] per ritentare"),
                    Status::Cancelled => "annullato".to_string(),
                };
                let info_color = match &status {
                    Status::Failed(_) => RED,
                    Status::Paused => AMBER,
                    Status::Done => MINT,
                    _ => MUTED,
                };
                ui.label(RichText::new(info).color(info_color).size(11.0));
            });
    }
}

/// Gauge con mappa segmenti stile torrent: ogni connessione riempie il suo pezzo.
fn seg_gauge(ui: &mut egui::Ui, dl: &Download, status: &Status, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, Rounding::same(3.0), TRACK);

    let total = dl.total.load(Ordering::Relaxed);
    let segs = dl.segs.lock().unwrap();
    if matches!(status, Status::Done) {
        p.rect_filled(rect.shrink(1.0), Rounding::same(2.0), MINT);
        return;
    }
    if total == 0 || segs.is_empty() {
        if dl.done.load(Ordering::Relaxed) > 0 && matches!(status, Status::Active) {
            p.rect_filled(rect.shrink(1.0), Rounding::same(2.0), CHROME);
        }
        return;
    }
    let colors = [MINT, BLUE];
    for (i, seg) in segs.iter().enumerate() {
        let done = seg.done.load(Ordering::Relaxed).min(seg.len());
        if done == 0 {
            continue;
        }
        let x0 = rect.left() + rect.width() * (seg.start as f32 / total as f32);
        let w = rect.width() * (done as f32 / total as f32);
        let r = Rect::from_min_size(pos2(x0, rect.top() + 1.0), vec2(w.max(1.0), rect.height() - 2.0));
        let color = match status {
            Status::Paused => AMBER,
            Status::Failed(_) => RED,
            _ => colors[i % 2],
        };
        p.rect_filled(r, Rounding::ZERO, color);
    }
}

/// Grafico a barre della velocità totale, stile pannello net del riferimento.
fn sparkline(ui: &mut egui::Ui, history: &VecDeque<f32>, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, Rounding::same(3.0), TRACK);
    if history.is_empty() {
        return;
    }
    let max = history.iter().cloned().fold(1.0_f32, f32::max);
    let n = history.len();
    let bar_w = (rect.width() / 150.0).max(2.0);
    for (i, v) in history.iter().enumerate() {
        if *v <= 0.0 {
            continue;
        }
        let h = (v / max * (rect.height() - 4.0)).max(1.0);
        let x = rect.right() - (n - i) as f32 * bar_w;
        if x < rect.left() {
            continue;
        }
        let color = if i % 2 == 0 { MINT } else { PINK };
        p.rect_filled(
            Rect::from_min_size(pos2(x, rect.bottom() - 2.0 - h), vec2(bar_w - 1.0, h)),
            Rounding::ZERO,
            color,
        );
    }
}

/// Finestra stile terminale: chrome [X][+][_] + titolo colorato + bordo.
fn term_window<R>(ui: &mut egui::Ui, title: &str, accent: Color32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(PANEL)
        .stroke(Stroke::new(1.5, PANEL_DARK))
        .rounding(Rounding::same(4.0))
        .inner_margin(egui::Margin::same(10.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(RichText::new("[").color(MUTED));
                ui.label(RichText::new("X").color(RED));
                ui.label(RichText::new("][").color(MUTED));
                ui.label(RichText::new("+").color(MINT));
                ui.label(RichText::new("][").color(MUTED));
                ui.label(RichText::new("_").color(AMBER));
                ui.label(RichText::new("] ").color(MUTED));
                ui.label(RichText::new(title).color(accent).strong());
            });
            let sep_y = ui.cursor().top() + 2.0;
            ui.painter().hline(ui.max_rect().x_range(), sep_y, Stroke::new(1.0, CHROME));
            ui.add_space(8.0);
            add(ui)
        })
        .inner
}

/// Tag/tab del "desktop". Ritorna true se cliccato.
fn desk_tag(ui: &mut egui::Ui, text: &str, active: bool) -> bool {
    let fill = if active { DESK_TAG } else { DESK_TAG_OFF };
    ui.add(
        egui::Button::new(RichText::new(text).color(DESK_TEXT))
            .fill(fill)
            .stroke(Stroke::new(1.0, DESK_TEXT))
            .rounding(Rounding::same(2.0)),
    )
    .clicked()
}

fn tbtn(ui: &mut egui::Ui, label: &str, color: Color32) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).color(color).size(11.0))
            .fill(TRACK)
            .stroke(Stroke::new(1.0, CHROME))
            .rounding(Rounding::same(2.0)),
    )
    .clicked()
}

// ---- topolino pixel ----

const PET_W: usize = 14;
const PET_F0: [&str; 7] = [
    "..........pp..",
    "..........pp..",
    "....oooooooo..",
    "p..oooooooooo.",
    ".ppoooooookop.",
    "...oooooooooo.",
    "....oo....oo..",
];
const PET_F1: [&str; 7] = [
    "..........pp..",
    "..........pp..",
    "....oooooooo..",
    ".p.oooooooooo.",
    "pp.oooooookop.",
    "...oooooooooo.",
    ".....oo..oo...",
];

/// Topolino pixel che corre da sinistra a destra in loop.
fn pet_strip(ui: &mut egui::Ui, t: f64) {
    let px = 2.0;
    let h = PET_F0.len() as f32 * px + 4.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let frame: &[&str; 7] = if (t * 8.0) as u64 % 2 == 0 { &PET_F0 } else { &PET_F1 };
    let sprite_w = PET_W as f32 * px;
    let span = rect.width() + sprite_w * 2.0;
    let x0 = rect.left() - sprite_w + ((t * 70.0) as f32 % span);
    let y0 = rect.top() + 2.0;
    let p = ui.painter();
    for (ry, row) in frame.iter().enumerate() {
        for (rx, c) in row.chars().enumerate() {
            let color = match c {
                'o' => PET_BODY,
                'p' => PINK,
                'k' => DESK_TEXT,
                _ => continue,
            };
            p.rect_filled(
                Rect::from_min_size(pos2(x0 + rx as f32 * px, y0 + ry as f32 * px), vec2(px, px)),
                Rounding::ZERO,
                color,
            );
        }
    }
}

pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{b} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn fmt_eta(secs: f64) -> String {
    let s = secs as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

fn reveal(path: &std::path::Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").args(["-R", &path.display().to_string()]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(std::path::Path::new("."))).spawn();
}

// ---- tray (solo Windows) ----

#[cfg(windows)]
enum TrayAction {
    None,
    Open,
    Quit,
}

#[cfg(windows)]
struct Tray {
    _icon: tray_icon::TrayIcon,
    open_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
}

#[cfg(windows)]
impl Tray {
    fn build(ctx: &egui::Context) -> Option<Self> {
        use tray_icon::menu::{Menu, MenuEvent, MenuItem};

        let open = MenuItem::new("Apri", true, None);
        let quit = MenuItem::new("Esci", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &quit]).ok()?;

        // sveglia il loop egui quando arriva un evento dal tray
        let ctx2 = ctx.clone();
        MenuEvent::set_event_handler(Some(move |_| ctx2.request_repaint()));
        let ctx3 = ctx.clone();
        tray_icon::TrayIconEvent::set_event_handler(Some(move |_| ctx3.request_repaint()));

        let icon = tray_icon::Icon::from_rgba(icon_rgba(), 32, 32).ok()?;
        let tray = tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("MDM — download manager")
            .with_icon(icon)
            .build()
            .ok()?;
        Some(Self { _icon: tray, open_id: open.id().clone(), quit_id: quit.id().clone() })
    }

    fn poll(&self) -> TrayAction {
        use tray_icon::menu::MenuEvent;
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.quit_id {
                return TrayAction::Quit;
            }
            if ev.id == self.open_id {
                return TrayAction::Open;
            }
        }
        TrayAction::None
    }
}

/// Freccia mint giù su sfondo trasparente, 32x32 RGBA.
#[cfg(windows)]
fn icon_rgba() -> Vec<u8> {
    let mut px = vec![0u8; 32 * 32 * 4];
    let mut set = |x: i32, y: i32| {
        if (0..32).contains(&x) && (0..32).contains(&y) {
            let i = ((y * 32 + x) * 4) as usize;
            px[i] = 0x9d;
            px[i + 1] = 0xe3;
            px[i + 2] = 0xc2;
            px[i + 3] = 255;
        }
    };
    for y in 4..18 {
        for x in 13..19 {
            set(x, y);
        }
    }
    for (row, y) in (18..26).enumerate() {
        let w = 12 - row as i32;
        for x in (16 - w)..(16 + w) {
            set(x, y);
        }
    }
    for x in 6..26 {
        for y in 27..30 {
            set(x, y);
        }
    }
    px
}
