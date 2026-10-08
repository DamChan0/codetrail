mod agentapp;
mod agents;
mod agents_fake;
mod agents_real;
mod agentui;
mod agentvm;
mod app;
mod diffmodel;
mod editor;
mod fonts;
mod fsutil;
mod highlight;
mod jobs;
mod projectapp;
mod railsplit;
mod resmon;
mod proc;
mod projects;
mod worktree;
mod wtapp;
mod runs_real;
mod selection;
mod settings;
mod theme;
mod timefmt;
mod ui;
mod views;
mod widgets;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "codetrail [REPO]\n       codetrail <install|hook|note|record|ask|export> ...\n       codetrail --smoke REPO --screenshot OUT.png [--scene commit|split|search|why|light|settings|runs|runs-stream|accounts|model-picker|new-run|project-picker|folder-browser|worktree|resources] [--size WxH] [--query TEXT]";

struct Args {
    repo: Option<PathBuf>,
    smoke: Option<(PathBuf, PathBuf, String, String)>,
    size: (f32, f32),
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut repo = None;
    let mut smoke_repo = None;
    let mut shot = None;
    let mut scene = "commit".to_string();
    let mut query = "fn".to_string();
    let mut size = (1440.0, 900.0);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--smoke" => smoke_repo = Some(PathBuf::from(val("--smoke")?)),
            "--screenshot" => shot = Some(PathBuf::from(val("--screenshot")?)),
            "--scene" => {
                scene = val("--scene")?;
                if !["commit", "split", "search", "why", "light", "settings", "runs", "runs-stream", "accounts", "model-picker", "new-run", "project-picker", "folder-browser", "worktree", "resources"].contains(&scene.as_str()) {
                    return Err(format!("unknown scene {scene:?}"));
                }
            }
            "--query" => query = val("--query")?,
            "--size" => {
                let v = val("--size")?;
                let (w, h) = v.split_once('x').ok_or("--size expects WxH")?;
                size = (w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?);
            }
            "-h" | "--help" => return Err(String::new()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s => repo = Some(PathBuf::from(s)),
        }
    }
    let smoke = match (smoke_repo, shot) {
        (Some(r), Some(o)) => Some((r, o, scene, query)),
        (None, None) => None,
        _ => return Err("--smoke and --screenshot go together".into()),
    };
    Ok(Args { repo, smoke, size })
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().is_some_and(|a| ct_agent::SUBCOMMANDS.contains(&a.as_str())) {
        return ct_agent::run_cli(&argv);
    }
    let args = match parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("error: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (repo_arg, smoke) = match args.smoke {
        Some((r, out, scene, query)) => (Some(r), Some((out, scene, query))),
        None => (args.repo, None),
    };
    let size = args.size;
    let viewport = egui::ViewportBuilder::default().with_inner_size([size.0, size.1]).with_min_inner_size([640.0, 420.0]).with_title("CodeTrail").with_app_id("codetrail");
    let opts = eframe::NativeOptions { viewport, ..Default::default() };
    let res = eframe::run_native(
        "CodeTrail",
        opts,
        Box::new(move |cc| {
            let mut banners = Vec::new();
            let (theme_file, tb) = theme::ThemeFile::load(&theme::theme_path());
            if let Some(b) = tb {
                banners.push((widgets::Level::Warn, b));
            }
            let (settings, sb) = if smoke.is_some() { (settings::Settings::default(), None) } else { settings::Settings::load(&settings::Settings::path()) };
            if let Some(b) = sb {
                banners.push((widgets::Level::Warn, b));
            }
            // No path argument: reopen the last project that still exists; none -> the app opens the picker.
            let repo_arg = repo_arg.clone().or_else(|| settings.recent.first().map(|r| PathBuf::from(&r.path))).unwrap_or_default();
            let choice = fonts::discover();
            let note = fonts::install(&cc.egui_ctx, &choice);
            let th = theme::Theme { file: theme_file.clone(), ui_font: settings.ui_font_size, code_font: settings.code_font_size };
            th.apply(&cc.egui_ctx);
            let svc: std::sync::Arc<dyn agents::AgentService> = if smoke.is_some() { std::sync::Arc::new(agents_fake::FakeAgents::seeded()) } else { std::sync::Arc::new(agents_real::RealAgents) };
            let smoke = smoke.map(|(out, scene, query)| app::Smoke { scene, out, query, started: std::time::Instant::now(), stable: 0, settled_at: None, requested: false, prepared: false });
            Ok(Box::new(app::App::new(cc.egui_ctx.clone(), repo_arg, theme_file, settings, banners, note, smoke, svc)))
        }),
    );
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
