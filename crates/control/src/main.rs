use std::io::{BufRead, Write};
fn run() -> Result<(), String> {
    let command = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    match command.as_str() {
        "" | "--help" | "help" => {
            println!(
                "wmctl status | subscribe | terminal | launcher | recorder | close | fullscreen | floating\n      recorder start|replay-start [output|window ID|region X Y W H]  (Screen capture)\n      recorder game-start PROFILE  (launch through an OpenGL or Vulkan API hook)\n      recorder game-attach PID  (inject into a running OpenGL process)\n      recorder xwayland-start WINDOW  (commit-paced compositor DMA-BUF capture)\n      recorder stop|toggle|pause|replay-save|status\n      Game Capture: Xwayland captures managed X11 windows without injection; OpenGL can launch or attach.\n      workspace N [OUTPUT] | send N | focus DIRECTION/ID | move DIRECTION\n      layout master/monocle | ratio DELTA | scratchpad send/show\n      reload | lock | quit | config-default | config-check"
            );
            Ok(())
        }
        "config-default" => {
            print!(
                "{}",
                toml::to_string_pretty(&wm_core::Config::default()).unwrap()
            );
            Ok(())
        }
        "config-check" => {
            wm_core::Config::load()?;
            println!("Configuration valid: {}", wm_core::config_path().display());
            Ok(())
        }
        "subscribe" => {
            let mut s = std::os::unix::net::UnixStream::connect(wm_core::socket_path()?)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer(
                &mut s,
                &wm_core::Request {
                    version: 1,
                    command,
                },
            )
            .map_err(|e| e.to_string())?;
            s.write_all(b"\n").map_err(|e| e.to_string())?;
            for l in std::io::BufReader::new(s).lines() {
                println!("{}", l.map_err(|e| e.to_string())?)
            }
            Ok(())
        }
        _ => {
            let r = wm_core::connect_command(&command)?;
            if !r.ok {
                return Err(r.error.unwrap_or_default());
            }
            println!("{}", serde_json::to_string_pretty(&r.state).unwrap());
            Ok(())
        }
    }
}
fn main() {
    if let Err(e) = run() {
        eprintln!("wmctl: {e}");
        std::process::exit(1)
    }
}
