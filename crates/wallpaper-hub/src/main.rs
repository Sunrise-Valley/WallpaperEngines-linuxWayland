// SPDX-License-Identifier: MIT

mod app;
mod color_wheel;
mod config;
mod i18n;
mod promo;
mod we_ui;
mod wp;

fn main() -> cosmic::iced::Result {
    // --version / --help answer without starting the UI, so a package smoke test
    // (`wallpaper-hub --version`) works in a headless shell with no display.
    if let Some(first) = std::env::args().nth(1) {
        match first.as_str() {
            "--version" | "-V" => {
                println!("wallpaper-hub {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "wallpaper-hub {} — COSMIC front-end for the native Wallpaper Engine kernel\n\n\
                     Usage: wallpaper-hub [--page <name>] [--apply-first] [--win-w N] [--win-h N]\n\n\
                     The UI talks to the running kernel over $XDG_RUNTIME_DIR/we-native.sock;\n\
                     start it with: systemctl --user enable --now wallpaper-native-kernel.service",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    // Get the system's preferred languages.
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();

    // Enable localizations to be applied.
    i18n::init(&requested_languages);

    // 支持 --page <名字> 直接打开某一页（脚本化验证）；--apply-first 自动应用第一个壁纸
    let flags = std::env::args().skip(1).collect::<Vec<_>>().join(" ");

    // Settings for configuring the application window and iced runtime.
    // 默认给个像样的尺寸：WE 观感是「左侧网格 + 右侧常驻详情面板」，太窄会挤成一列。
    let mut settings = cosmic::app::Settings::default()
        .size(cosmic::iced::Size::new(1400.0, 900.0))
        .size_limits(
            cosmic::iced::Limits::NONE
                .min_width(900.0)
                .min_height(560.0),
        );

    // win-w / --win-h <像素>：只给**截图**用 —— 把窗口放到想要的尺寸再截
    // （本机没有输入模拟工具，改不了窗口大小）。两个都省 = 用默认尺寸。
    let argv: Vec<String> = std::env::args().collect();
    let arg_of = |name: &str| -> Option<f32> {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .and_then(|s| s.parse::<f32>().ok())
    };
    let win_w = arg_of("--win-w");
    let win_h = arg_of("--win-h");
    if win_w.is_some() || win_h.is_some() {
        let w = win_w.unwrap_or(1400.0);
        let h = win_h.unwrap_or(900.0);
        settings = settings.size(cosmic::iced::Size::new(w, h));
    }

    // 诊断：`WPHUB_DUMP_THEME=1` 打印主题各层的 alpha（核对半透明用的是哪一档）
    we_ui::dump_theme_layers();

    // 诊断：任何线程 panic 都要留下痕迹（std 默认只打 panic! 那一行到 stderr，
    // 而 systemd 下 stderr 也可能被 journal 吞掉；这里显式带线程名 + backtrace）。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!(
            "[panic] 线程 {:?}: {info}",
            std::thread::current().name().unwrap_or("<unnamed>")
        );
        default_hook(info);
    }));

    // Starts the application's event loop with the CLI flags as flags.
    let t0 = std::time::Instant::now();
    let res = cosmic::app::run::<app::AppModel>(settings, flags);
    // 事件循环**返回**是"静默退出"的唯一看不见的出口：run 正常返回 ⇒ 进程退出码 0、
    // 没有任何 panic/error（journal 里只剩 systemd 的 Consumed 行）。这里把它钉出来。
    eprintln!(
        "[exit] cosmic::app::run 返回 {:?} · 存活 {:.1}s",
        res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
        t0.elapsed().as_secs_f64()
    );
    res
}
