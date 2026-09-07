static POSSIBLE_BACKENDS: &[&str] = &[
    #[cfg(feature = "winit")]
    "--winit : Run wm as an X11 or Wayland client using winit.",
    #[cfg(feature = "udev")]
    "--tty-udev : Run wm on a TTY using libseat (use tools/run-tty.sh).",
    #[cfg(feature = "x11")]
    "--x11 : Run wm in a nested X11 window.",
];

#[cfg(feature = "profile-with-tracy-mem")]
#[global_allocator]
static GLOBAL: profiling::tracy_client::ProfiledAllocator<std::alloc::System> =
    profiling::tracy_client::ProfiledAllocator::new(std::alloc::System, 10);

// Allow in this function because of existing usage
#[allow(clippy::uninlined_format_args)]
fn main() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt()
            .compact()
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt().compact().init();
    }

    #[cfg(feature = "profile-with-tracy")]
    profiling::tracy_client::Client::start();

    profiling::register_thread!("Main Thread");

    #[cfg(feature = "profile-with-puffin")]
    let _server =
        puffin_http::Server::new(&format!("0.0.0.0:{}", puffin_http::DEFAULT_PORT)).unwrap();
    #[cfg(feature = "profile-with-puffin")]
    profiling::puffin::set_scopes_on(true);

    let arg = ::std::env::args().nth(1);
    match arg.as_ref().map(|s| &s[..]) {
        #[cfg(feature = "winit")]
        Some("--winit") => {
            tracing::info!("Starting wm with winit backend");
            wm_compositor::winit::run_winit();
        }
        #[cfg(feature = "udev")]
        Some("--tty-udev") => {
            tracing::info!("Starting wm on a tty using udev");
            wm_compositor::udev::run_udev();
        }
        #[cfg(feature = "x11")]
        Some("--x11") => {
            tracing::info!("Starting wm with x11 backend");
            wm_compositor::x11::run_x11();
        }
        Some(other) => {
            tracing::error!("Unknown backend: {}", other);
        }
        None => {
            #[allow(clippy::disallowed_macros)]
            {
                println!("USAGE: wm --backend");
                println!();
                println!("Possible backends are:");
                for b in POSSIBLE_BACKENDS {
                    println!("\t{b}");
                }
            }
        }
    }
}
