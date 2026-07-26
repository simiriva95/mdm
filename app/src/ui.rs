//! UI stile "desktop di terminali": sfondo chiaro, finestre navy con chrome
//! colorato [X][+][_], gauge pastello con mappa segmenti, sparkline di rete,
//! tab Downloads/Console e topolino pixel che corre in basso.

use std::collections::{HashMap, HashSet, VecDeque};
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
            .with_icon(egui::IconData { rgba: icon_rgba(), width: 32, height: 32 })
            .with_title("MDM"),
        ..Default::default()
    };
    eframe::run_native(
        "MDM",
        options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            *state.egui_ctx.lock().unwrap() = Some(cc.egui_ctx.clone());
            // HWND per riaprire la finestra dal tray anche a loop egui fermo
            #[cfg(windows)]
            {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                if let Ok(h) = cc.window_handle() {
                    if let RawWindowHandle::Win32(w) = h.as_raw() {
                        state.hwnd.store(w.hwnd.get(), Ordering::Relaxed);
                    }
                }
            }
            for line in [
                r" __  __ ____  __  __ ",
                r"|  \/  |  _ \|  \/  |",
                r"| |\/| | | | | |\/| |",
                r"| |  | | |_| | |  | |",
                r"|_|  |_|____/|_|  |_|",
            ] {
                state.log(line);
            }
            state.log(format!("MDM v{} pronto — in ascolto su 127.0.0.1:48666", env!("CARGO_PKG_VERSION")));
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
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32,CHROME);
    style.visuals.widgets.inactive.bg_fill = PANEL_DARK;
    style.visuals.widgets.inactive.weak_bg_fill = PANEL_DARK;
    style.visuals.widgets.hovered.bg_fill = CHROME;
    style.visuals.widgets.hovered.weak_bg_fill = CHROME;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32,MUTED);
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
    Settings,
}

struct SpeedSample {
    at: Instant,
    bytes: u64,
    speed: f64, // byte/s, media mobile esponenziale
}

/// Coriandolo pixel: traiettoria balistica calcolata dall'età, zero stato per frame.
struct Confetti {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    color: Color32,
    size: f32,
    born: f64,
}

const CONFETTI_LIFE: f64 = 1.5;

struct App {
    state: Arc<AppState>,
    tab: Tab,
    speeds: HashMap<u64, SpeedSample>,
    net_history: VecDeque<f32>,
    net_peak: f64,
    last_net_sample: Instant,
    confetti: Vec<Confetti>,
    celebrated: HashSet<u64>, // download già festeggiati
    /// buffer del campo cartella: la config tiene un PathBuf, la UI una String
    dir_buf: String,
    #[cfg(windows)]
    _tray: Option<Tray>,
    quitting: bool,
}

impl App {
    fn new(state: Arc<AppState>, _ctx: &egui::Context) -> Self {
        Self {
            state: state.clone(),
            tab: Tab::Downloads,
            speeds: HashMap::new(),
            net_history: VecDeque::with_capacity(160),
            net_peak: 0.0,
            last_net_sample: Instant::now(),
            confetti: Vec::new(),
            celebrated: HashSet::new(),
            dir_buf: state.config.get().download_dir.to_string_lossy().into_owned(),
            #[cfg(windows)]
            _tray: Tray::build(state),
            quitting: false,
        }
    }

    fn speed_of(&mut self, id: u64, done: u64) -> f64 {
        let now = Instant::now();
        let s = self.speeds.entry(id).or_insert(SpeedSample { at: now, bytes: done, speed: 0.0 });
        let dt = now.duration_since(s.at).as_secs_f64();
        if dt >= 0.5 {
            let inst = (done.saturating_sub(s.bytes)) as f64 / dt;
            // EMA: velocità e eta stabili invece che ballerine
            s.speed = if s.speed == 0.0 { inst } else { 0.3 * inst + 0.7 * s.speed };
            if inst == 0.0 && s.speed < 1024.0 {
                s.speed = 0.0; // sotto 1KB/s mostra 0 invece di code infinite
            }
            s.at = now;
            s.bytes = done;
        }
        s.speed
    }

    /// Esplosione di coriandoli pixel dal rettangolo dato.
    fn spawn_confetti(&mut self, rect: Rect, t: f64) {
        let colors = [MINT, PINK, AMBER, BLUE];
        for i in 0..36 {
            // pseudo-random deterministico, niente dipendenza rand
            let f1 = ((i as f32 * 12.9898).sin() * 43758.547).fract().abs();
            let f2 = ((i as f32 * 78.233).sin() * 24634.629).fract().abs();
            self.confetti.push(Confetti {
                x: rect.left() + rect.width() * (i as f32 + 0.5) / 36.0,
                y: rect.top() + 6.0,
                vx: (f1 - 0.5) * 190.0,
                vy: -(50.0 + f2 * 170.0),
                color: colors[i % colors.len()],
                size: if i % 3 == 0 { 4.0 } else { 3.0 },
                born: t,
            });
        }
    }

    /// Disegna e pota i coriandoli sul layer in primo piano.
    fn paint_confetti(&mut self, ctx: &egui::Context, t: f64) {
        self.confetti.retain(|c| t - c.born < CONFETTI_LIFE);
        if self.confetti.is_empty() {
            return;
        }
        let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("confetti")));
        for c in &self.confetti {
            let age = (t - c.born) as f32;
            let x = c.x + c.vx * age;
            let y = c.y + c.vy * age + 0.5 * 320.0 * age * age; // gravità
            let fade = 1.0 - (age / CONFETTI_LIFE as f32);
            let color = Color32::from_rgba_unmultiplied(c.color.r(), c.color.g(), c.color.b(), (fade * 255.0) as u8);
            p.rect_filled(Rect::from_min_size(pos2(x, y), vec2(c.size, c.size)), Rounding::ZERO, color);
        }
        ctx.request_repaint(); // animazione fluida finché ci sono coriandoli
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.state.show_window.swap(false, Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        // "Esci" dal tray: il handler setta il flag e risveglia il loop
        if self.state.quit.load(Ordering::Relaxed) {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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

        // barra "desktop" in alto: nuvole pixel + tag + orologio
        egui::TopBottomPanel::top("desk_top").frame(egui::Frame::none().fill(DESK)).show(ctx, |ui| {
            pixel_clouds(ui, ctx.input(|i| i.time));
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
                if desk_tag(ui, "[ Settings ]", self.tab == Tab::Settings) {
                    self.tab = Tab::Settings;
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

                    // update disponibile: tag lampeggiante, click = installa
                    let upd = self.state.update.lock().unwrap().as_ref().map(|u| u.version.clone());
                    if let Some(ver) = upd {
                        let t = ctx.input(|i| i.time);
                        let busy = self.state.updating.load(Ordering::Relaxed);
                        let label = if busy { "[ aggiorno... ]".to_string() } else { format!("[ ⇡ update {ver} ]") };
                        let color = if busy || (t * 2.0) as u64 % 2 == 0 { AMBER } else { DESK_TEXT };
                        ui.add_space(8.0);
                        if ui
                            .add(
                                egui::Button::new(RichText::new(label).color(DESK_TEXT))
                                    .fill(if color == AMBER { AMBER } else { DESK_TAG_OFF })
                                    .stroke(Stroke::new(1.0_f32,DESK_TEXT))
                                    .rounding(Rounding::same(2.0)),
                            )
                            .on_hover_text("scarica e installa la nuova versione, poi riavvia")
                            .clicked()
                            && !busy
                        {
                            let rt = self.state.rt.lock().unwrap().clone();
                            if let Some(rt) = rt {
                                rt.spawn(crate::update::apply(self.state.clone()));
                            }
                        }
                    }
                });
            });
            ui.add_space(3.0);
        });

        egui::CentralPanel::default().frame(egui::Frame::none().fill(DESK).inner_margin(6.0)).show(ctx, |ui| {
            match self.tab {
                Tab::Console => self.console_tab(ui),
                Tab::Settings => self.settings_tab(ui),
                Tab::Downloads => self.downloads_tab(ui, &downloads, n_active, total_speed),
            }
        });

        // salvataggio pigro della config: gli slider muovono i valori a ogni
        // frame, il write su disco avviene solo quando si smette di toccarli
        self.state.config.flush_due();

        self.paint_confetti(ctx, ctx.input(|i| i.time));

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
                    let t = ui.input(|i| i.time);
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        // topolino che dorme con le z animate
                        sleeping_pet(ui, t);
                        ui.add_space(4.0);
                        ui.label(RichText::new("┌──────────────────────────────────┐").color(CHROME));
                        ui.label(RichText::new("│  in attesa di download da Chrome  │").color(MUTED));
                        ui.label(RichText::new("└──────────────────────────────────┘").color(CHROME));
                        ui.add_space(6.0);
                        ui.label(RichText::new("icona estensione = ON/OFF · >10MB passano di qui").color(CHROME).size(11.0));
                    });
                    return;
                }
                // controlli globali: pausa/riprendi tutti
                let any_active = downloads
                    .iter()
                    .any(|d| matches!(*d.status.lock().unwrap(), Status::Active | Status::Connecting));
                let any_resumable = downloads
                    .iter()
                    .any(|d| matches!(*d.status.lock().unwrap(), Status::Paused | Status::Failed(_)));
                if any_active || any_resumable {
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if any_active && tbtn(ui, "[ ⏸ pausa tutti ]", AMBER) {
                                for d in downloads {
                                    if matches!(*d.status.lock().unwrap(), Status::Active | Status::Connecting) {
                                        d.pause.store(true, Ordering::Relaxed);
                                    }
                                }
                            }
                            if any_resumable && tbtn(ui, "[ ▶ riprendi tutti ]", MINT) {
                                let rt = self.state.rt.lock().unwrap().clone();
                                if let Some(rt) = rt {
                                    for d in downloads {
                                        if matches!(*d.status.lock().unwrap(), Status::Paused | Status::Failed(_)) {
                                            rt.spawn(engine::resume(self.state.clone(), d.clone()));
                                        }
                                    }
                                }
                            }
                        });
                    });
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

    /// Impostazioni: stessa estetica delle altre finestre, una riga per voce.
    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let h = ui.available_height();
        let mut cfg = self.state.config.get();
        let before = cfg.clone();

        ui.allocate_ui(vec2(ui.available_width(), h), |ui| {
            term_window(ui, "settings", AMBER, |ui| {
                ui.set_min_height(h - 50.0);
                egui::ScrollArea::vertical().id_salt("settings_scroll").show(ui, |ui| {
                    setting_row(ui, "cartella", "vuoto = cartella Downloads di sistema", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.dir_buf).desired_width(320.0).hint_text("Downloads"));
                        if tbtn(ui, "[ apri ]", BLUE) {
                            if let Ok(d) = self.state.dl_dir() {
                                reveal(&d.join("."));
                            }
                        }
                    });
                    cfg.download_dir = std::path::PathBuf::from(self.dir_buf.trim());

                    setting_row(ui, "connessioni", "per download; troppe fanno scattare i 429", |ui| {
                        ui.add(egui::Slider::new(&mut cfg.max_connections, 1..=engine::MAX_CONNECTIONS).integer());
                    });

                    setting_row(ui, "download insieme", "gli altri restano in coda", |ui| {
                        ui.add(egui::Slider::new(&mut cfg.max_concurrent_downloads, 1..=10).integer());
                    });

                    setting_row(ui, "limite banda", "KB/s totali, 0 = illimitato", |ui| {
                        ui.add(egui::Slider::new(&mut cfg.speed_limit_kbps, 0..=100_000).integer().logarithmic(true));
                    });

                    setting_row(ui, "soglia estensione", "MB oltre cui Chrome passa il download a MDM", |ui| {
                        ui.add(egui::Slider::new(&mut cfg.size_threshold_mb, 1..=1000).integer().logarithmic(true));
                    });

                    setting_row(ui, "retry automatici", "quante volte ritentare da solo dopo un errore", |ui| {
                        ui.add(egui::Slider::new(&mut cfg.auto_retry, 0..=10).integer());
                    });

                    setting_row(ui, "notifiche", "avviso di sistema a download finito", |ui| {
                        ui.checkbox(&mut cfg.notify_on_complete, "");
                    });

                    setting_row(ui, "guarda appunti", "proponi il download se copi un link a un file", |ui| {
                        ui.checkbox(&mut cfg.clipboard_watch, "");
                    });

                    #[cfg(windows)]
                    setting_row(ui, "avvio automatico", "parte con Windows, minimizzato nel tray", |ui| {
                        if ui.checkbox(&mut cfg.autostart, "").changed() {
                            if let Err(e) = crate::config::set_autostart(cfg.autostart) {
                                self.state.log(format!("ERRORE autostart: {e:#}"));
                            }
                        }
                    });

                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!("config: {}", crate::config::path().display()))
                            .color(CHROME)
                            .size(10.0),
                    );
                    if !cfg.host_conc.is_empty() {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{} host rate-limitati ricordati", cfg.host_conc.len()))
                                    .color(CHROME)
                                    .size(10.0),
                            );
                            if tbtn(ui, "[ dimentica ]", MUTED) {
                                cfg.host_conc.clear();
                            }
                        });
                    }
                });
            });
        });

        if cfg != before {
            self.state.config.edit(move |c| *c = cfg);
        }
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
                        // il log a schermo è solo le ultime 200 righe: il file ha tutto
                        if tbtn(ui, "[ apri log ]", BLUE) {
                            reveal(&engine::log_path());
                        }
                    });
                });
                egui::ScrollArea::vertical().id_salt("log_full").stick_to_bottom(true).show(ui, |ui| {
                    for line in self.state.log.lock().unwrap().iter() {
                        let color = if line.contains("ERRORE") {
                            RED
                        } else if line.contains("completato") {
                            MINT
                        } else if line.contains("aggiornamento") {
                            AMBER
                        } else {
                            MUTED
                        };
                        ui.label(RichText::new(line).color(color).size(12.0));
                    }
                    // prompt con cursore a blocco lampeggiante
                    let t = ui.input(|i| i.time);
                    let cursor = if (t * 2.0) as u64 % 2 == 0 { "█" } else { " " };
                    ui.label(
                        RichText::new(format!("mdm> {cursor}")).color(TEXT).size(12.0),
                    );
                });
            });
        });
    }

    fn download_row(&mut self, ui: &mut egui::Ui, dl: &Arc<Download>, to_remove: &mut Vec<u64>) {
        let done = dl.done.load(Ordering::Relaxed);
        let total = dl.total.load(Ordering::Relaxed);
        let status = dl.status.lock().unwrap().clone();
        let name = dl.name.lock().unwrap().clone();
        let url = dl.job.lock().unwrap().url.clone();
        let speed = self.speed_of(dl.id, done);

        let row = egui::Frame::none()
            .fill(PANEL_DARK)
            .rounding(Rounding::same(4.0))
            .inner_margin(egui::Margin::same(8.0))
            .outer_margin(egui::Margin::symmetric(0.0, 3.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let t = ui.input(|i| i.time);
                    let spin = ["|", "/", "-", "\\"][(t * 8.0) as usize % 4];
                    let (dot, color) = match status {
                        Status::Active => (spin, MINT),
                        Status::Connecting => (spin, AMBER),
                        Status::Paused => ("⏸", AMBER),
                        Status::Done => ("✔", MINT),
                        Status::Failed(_) => ("✗", RED),
                        Status::Cancelled => ("–", MUTED),
                    };
                    ui.label(RichText::new(dot).color(color).strong());
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

                // link sorgente: click = copia negli appunti
                ui.horizontal(|ui| {
                    ui.label(RichText::new("↗").color(CHROME).size(10.0));
                    let resp = ui
                        .add(
                            egui::Label::new(RichText::new(&url).color(BLUE).size(10.0))
                                .truncate()
                                .sense(Sense::click()),
                        )
                        .on_hover_text("click: copia il link");
                    if resp.clicked() {
                        ui.ctx().copy_text(url.clone());
                        self.state.log(format!("link copiato: {url}"));
                    }
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

        // festeggia il completamento: coriandoli pixel una volta sola
        if matches!(status, Status::Done) && self.celebrated.insert(dl.id) {
            let t = ui.input(|i| i.time);
            self.spawn_confetti(row.response.rect, t);
        }
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
    // griglia orizzontale da oscilloscopio: 25/50/75%
    let grid = Color32::from_rgba_unmultiplied(0x8d, 0x94, 0xb8, 26);
    for frac in [0.25f32, 0.5, 0.75] {
        p.hline(rect.x_range().shrink(2.0), rect.bottom() - rect.height() * frac, Stroke::new(1.0_f32,grid));
    }
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

/// Finestra stile terminale: chrome [X][+][_] + titolo colorato + bordo,
/// con scanline CRT sopra il contenuto.
fn term_window<R>(ui: &mut egui::Ui, title: &str, accent: Color32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let out = egui::Frame::none()
        .fill(PANEL)
        .stroke(Stroke::new(1.5_f32,PANEL_DARK))
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
            ui.painter().hline(ui.max_rect().x_range(), sep_y, Stroke::new(1.0_f32,CHROME));
            ui.add_space(8.0);
            add(ui)
        });
    // scanline CRT: righe scure appena percettibili ogni 3px
    let rect = out.response.rect;
    let p = ui.painter();
    let scan = Color32::from_rgba_unmultiplied(0, 0, 0, 14);
    let mut y = rect.top() + 1.0;
    while y < rect.bottom() {
        p.hline(rect.x_range().shrink(2.0), y, Stroke::new(1.0_f32,scan));
        y += 3.0;
    }
    out.inner
}

/// Tag/tab del "desktop". Ritorna true se cliccato.
fn desk_tag(ui: &mut egui::Ui, text: &str, active: bool) -> bool {
    let fill = if active { DESK_TAG } else { DESK_TAG_OFF };
    ui.add(
        egui::Button::new(RichText::new(text).color(DESK_TEXT))
            .fill(fill)
            .stroke(Stroke::new(1.0_f32,DESK_TEXT))
            .rounding(Rounding::same(2.0)),
    )
    .clicked()
}

/// Riga di impostazione: etichetta a sinistra, widget a destra, nota sotto.
fn setting_row(ui: &mut egui::Ui, label: &str, hint: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.allocate_ui(vec2(150.0, 18.0), |ui| {
            ui.label(RichText::new(label).color(TEXT).strong());
        });
        add(ui);
    });
    ui.horizontal(|ui| {
        ui.add_space(150.0);
        ui.label(RichText::new(hint).color(CHROME).size(10.0));
    });
}

fn tbtn(ui: &mut egui::Ui, label: &str, color: Color32) -> bool {
    ui.add(
        egui::Button::new(RichText::new(label).color(color).size(11.0))
            .fill(TRACK)
            .stroke(Stroke::new(1.0_f32,CHROME))
            .rounding(Rounding::same(2.0)),
    )
    .clicked()
}

// ---- pixel art ----

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

// topolino addormentato (fermo, orecchie giù)
const PET_SLEEP: [&str; 7] = [
    "..............",
    "..............",
    "....oooooooo..",
    "p..oooooooooo.",
    ".ppoooooo-oop.",
    "...oooooooooo.",
    "...oooooooo...",
];

// gatto che insegue: corpo ambra, orecchie, occhi, coda su
const CAT_F0: [&str; 7] = [
    ".a...a.......a...",
    ".aa.aa......aa...",
    ".aaaaa.....aa....",
    ".akaka..aaaaa....",
    ".aaaaaaaaaaaa....",
    ".aaaaaaaaaa......",
    "..aa....aa.......",
];
const CAT_F1: [&str; 7] = [
    ".a...a........a..",
    ".aa.aa.......aa..",
    ".aaaaa.....aa....",
    ".akaka..aaaaa....",
    ".aaaaaaaaaaaa....",
    ".aaaaaaaaaa......",
    "...aa..aa........",
];

fn sprite_color(c: char) -> Option<Color32> {
    match c {
        'o' => Some(PET_BODY),
        'a' => Some(AMBER),
        'p' => Some(PINK),
        'k' => Some(DESK_TEXT),
        '-' => Some(DESK_TEXT), // occhio chiuso
        _ => None,
    }
}

fn draw_sprite(p: &egui::Painter, frame: &[&str], x0: f32, y0: f32, px: f32) {
    for (ry, row) in frame.iter().enumerate() {
        for (rx, c) in row.chars().enumerate() {
            let Some(color) = sprite_color(c) else { continue };
            p.rect_filled(
                Rect::from_min_size(pos2(x0 + rx as f32 * px, y0 + ry as f32 * px), vec2(px, px)),
                Rounding::ZERO,
                color,
            );
        }
    }
}

/// Striscia in basso: gatto che insegue il topolino, in loop.
fn pet_strip(ui: &mut egui::Ui, t: f64) {
    let px = 2.0;
    let h = PET_F0.len() as f32 * px + 4.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let p = ui.painter();
    let run = (t * 8.0) as u64 % 2 == 0;

    let sprite_w = PET_W as f32 * px;
    let span = rect.width() + sprite_w * 2.0 + 90.0;
    let mouse_x = rect.left() - sprite_w + ((t * 70.0) as f32 % span);
    let y0 = rect.top() + 2.0;
    draw_sprite(p, if run { &PET_F0 } else { &PET_F1 }, mouse_x, y0, px);

    // il gatto arranca dietro, distanza che respira: non lo prende mai
    let gap = 52.0 + 10.0 * ((t * 1.7).sin() as f32);
    let cat_x = mouse_x - gap;
    draw_sprite(p, if run { &CAT_F1 } else { &CAT_F0 }, cat_x, y0, px);
}

/// Topolino che dorme con le z che salgono (stato vuoto).
fn sleeping_pet(ui: &mut egui::Ui, t: f64) {
    let px = 3.0;
    let w = PET_W as f32 * px + 40.0;
    let h = PET_SLEEP.len() as f32 * px + 14.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let x0 = rect.center().x - PET_W as f32 * px / 2.0;
    let y0 = rect.bottom() - PET_SLEEP.len() as f32 * px;
    let p = ui.painter();
    draw_sprite(p, &PET_SLEEP, x0, y0, px);
    // z z Z che fluttuano a turno
    let phase = (t * 1.2).fract() as f32;
    for (i, size) in [(0usize, 9.0f32), (1, 11.0), (2, 13.0)] {
        let stage = ((t * 1.2) as usize + i) % 3;
        let dy = phase * 4.0;
        p.text(
            pos2(x0 + PET_W as f32 * px + 4.0 + stage as f32 * 7.0, y0 - 2.0 - stage as f32 * 8.0 - dy),
            egui::Align2::LEFT_BOTTOM,
            "z",
            FontId::monospace(size),
            Color32::from_rgba_unmultiplied(MUTED.r(), MUTED.g(), MUTED.b(), 160),
        );
    }
}

// nuvole pixel che attraversano la barra desktop in alto
const CLOUD: [&str; 3] = [
    "...cccc....",
    ".cccccccc..",
    "ccccccccccc",
];

fn pixel_clouds(ui: &egui::Ui, t: f64) {
    let rect = ui.max_rect();
    let p = ui.painter();
    let px = 2.0;
    let cloud_w = CLOUD[0].len() as f32 * px;
    for (i, speed) in [(0u32, 7.0f32), (1, 11.0), (2, 5.0)] {
        let span = rect.width() + cloud_w * 2.0;
        let x0 = rect.left() - cloud_w + ((t as f32 * speed) + i as f32 * span / 3.0) % span;
        let y0 = rect.top() + 3.0 + (i as f32 * 4.0) % 8.0;
        for (ry, row) in CLOUD.iter().enumerate() {
            for (rx, c) in row.chars().enumerate() {
                if c != 'c' {
                    continue;
                }
                p.rect_filled(
                    Rect::from_min_size(pos2(x0 + rx as f32 * px, y0 + ry as f32 * px), vec2(px, px)),
                    Rounding::ZERO,
                    DESK_TAG_OFF,
                );
            }
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
//
// I handler girano sul message pump di win32 (thread main), che resta vivo
// anche quando la finestra è nascosta e il loop egui è congelato (una finestra
// nascosta non riceve WM_PAINT, quindi update() non gira mai). Per questo
// l'apertura passa da wake_and_show(): ShowWindow via Win32 forza il repaint
// e il loop riparte. Era questo il bug "apri dal tray non fa nulla".

#[cfg(windows)]
struct Tray {
    _icon: tray_icon::TrayIcon,
}

#[cfg(windows)]
impl Tray {
    fn build(state: Arc<AppState>) -> Option<Self> {
        use tray_icon::menu::{Menu, MenuEvent, MenuItem};
        use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

        let open = MenuItem::new("Apri", true, None);
        let quit = MenuItem::new("Esci", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &quit]).ok()?;
        let open_id = open.id().clone();
        let quit_id = quit.id().clone();

        let st = state.clone();
        MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
            if ev.id == open_id {
                st.wake_and_show();
            } else if ev.id == quit_id {
                st.quit.store(true, Ordering::Relaxed);
                st.wake_and_show(); // il loop deve girare per processare Close
            }
        }));

        // click sinistro sull'icona = apri
        let st = state.clone();
        TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                st.wake_and_show();
            }
        }));

        let icon = tray_icon::Icon::from_rgba(icon_rgba(), 32, 32).ok()?;
        let tray = tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("MDM — download manager")
            .with_icon(icon)
            .build()
            .ok()?;
        Some(Self { _icon: tray })
    }
}

/// Icona 32x32 RGBA: il topolino pixel dell'app, ingrandito 2x e centrato.
fn icon_rgba() -> Vec<u8> {
    let mut px = vec![0u8; 32 * 32 * 4];
    let scale = 2usize;
    let off_x = (32 - PET_W * scale) / 2;
    let off_y = (32 - PET_F0.len() * scale) / 2;
    for (ry, row) in PET_F0.iter().enumerate() {
        for (rx, c) in row.chars().enumerate() {
            let Some(color) = sprite_color(c) else { continue };
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = off_x + rx * scale + dx;
                    let y = off_y + ry * scale + dy;
                    let i = (y * 32 + x) * 4;
                    px[i] = color.r();
                    px[i + 1] = color.g();
                    px[i + 2] = color.b();
                    px[i + 3] = 255;
                }
            }
        }
    }
    px
}
