use std::process::Command;

use anyhow::Result;

pub fn cmd_open() -> Result<()> {
    // Ensure daemon is running before opening browser
    if let Err(e) = crate::client::DaemonClient::connect() {
        anyhow::bail!("Could not connect to budi daemon: {e}\nRun `budi init` or `budi doctor` to diagnose.");
    }

    // Brief delay to let the dashboard endpoint settle
    std::thread::sleep(std::time::Duration::from_millis(200));

    let config = crate::client::DaemonClient::load_config();
    let url = format!("{}/dashboard", config.daemon_base_url());

    // Cross-platform browser launch
    let result = {
        #[cfg(target_os = "macos")]
        {
            Command::new("open").arg(&url).spawn()
        }
        #[cfg(target_os = "linux")]
        {
            Command::new("xdg-open").arg(&url).spawn()
        }
        #[cfg(target_os = "windows")]
        {
            Command::new("cmd").args(["/C", "start", &url]).spawn()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "unsupported platform",
            ))
        }
    };

    if let Err(e) = result {
        println!("{}", url);
        eprintln!("Could not open browser automatically: {e}");
        eprintln!("Open the URL above in your browser.");
    }

    Ok(())
}
