//! YouTube Live Translator — mini-navigateur Rust (tao + wry) qui lit une vidéo YouTube
//! et affiche des sous-titres générés/traduits en arabe, français, anglais, allemand, turc et espagnol.
//!
//! `ytlt`           ouvre la fenêtre
//! `ytlt --server`  lance uniquement le serveur local (pour tester dans un navigateur)

mod server;
mod subs;
mod translate;
mod whisper;
mod youtube;

use std::net::SocketAddr;

use anyhow::Result;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::{ResizeDirection, WindowBuilder};
use wry::WebViewBuilder;

const PREFERRED_PORT: u16 = 47653;

#[derive(Debug)]
enum Ui {
    Drag,
    Resize,
    Minimize,
    ToggleMaximize,
    ToggleFullscreen,
    Close,
}

/// Démarre le serveur sur un port fixe (pour que le localStorage de l'interface persiste),
/// ou sur un port libre si celui-ci est occupé.
fn start_server() -> Result<SocketAddr> {
    let std_listener = std::net::TcpListener::bind(("127.0.0.1", PREFERRED_PORT))
        .or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))?;
    std_listener.set_nonblocking(true)?;
    let addr = std_listener.local_addr()?;
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("runtime tokio");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(std_listener).expect("listener");
            axum::serve(listener, server::router()).await.expect("serveur");
        });
    });
    Ok(addr)
}

fn main() -> Result<()> {
    let addr = start_server()?;
    let url = format!("http://{addr}/");

    if std::env::args().any(|a| a == "--server") {
        println!("Serveur prêt : {url}");
        loop {
            std::thread::park();
        }
    }

    let event_loop = EventLoopBuilder::<Ui>::with_user_event().build();
    let _menu = app_menu();

    let window = WindowBuilder::new()
        .with_title("YouTube Live Translator")
        .with_decorations(false)
        .with_inner_size(LogicalSize::new(1180.0, 800.0))
        .with_min_inner_size(LogicalSize::new(760.0, 560.0))
        .build(&event_loop)?;

    let proxy = event_loop.create_proxy();
    let _webview = WebViewBuilder::new()
        .with_url(&url)
        .with_autoplay(true)
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |req| {
            let msg = match req.body().as_str() {
                "drag" => Ui::Drag,
                "resize" => Ui::Resize,
                "min" => Ui::Minimize,
                "max" => Ui::ToggleMaximize,
                "fs" => Ui::ToggleFullscreen,
                "close" => Ui::Close,
                _ => return,
            };
            let _ = proxy.send_event(msg);
        })
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(ui) => match ui {
                Ui::Drag => {
                    let _ = window.drag_window();
                }
                Ui::Resize => {
                    let _ = window.drag_resize_window(ResizeDirection::SouthEast);
                }
                Ui::Minimize => window.set_minimized(true),
                Ui::ToggleMaximize => window.set_maximized(!window.is_maximized()),
                Ui::ToggleFullscreen => {
                    let fs = window.fullscreen().is_some();
                    window.set_fullscreen((!fs).then_some(tao::window::Fullscreen::Borderless(None)));
                }
                Ui::Close => *control_flow = ControlFlow::Exit,
            },
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
}

/// Sans menu « Édition », macOS ne route pas ⌘C / ⌘V / ⌘A vers la webview.
fn app_menu() -> muda::Menu {
    use muda::{Menu, PredefinedMenuItem as P, Submenu};
    let menu = Menu::new();
    let app = Submenu::with_items("YouTube Live Translator", true, &[&P::about(None, None), &P::separator(), &P::hide(None), &P::quit(None)])
        .expect("menu");
    let edit = Submenu::with_items(
        "Édition",
        true,
        &[&P::undo(None), &P::redo(None), &P::separator(), &P::cut(None), &P::copy(None), &P::paste(None), &P::select_all(None)],
    )
    .expect("menu");
    let _ = menu.append_items(&[&app, &edit]);
    #[cfg(target_os = "macos")]
    menu.init_for_nsapp();
    menu
}
