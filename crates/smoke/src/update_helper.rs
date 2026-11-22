//! No Tauri or WebView is initialized in the trusted update helper process.
#[cfg(target_os = "macos")]
pub fn dispatch() -> Result<bool, String> {
    let args: Vec<_> = std::env::args_os().collect();
    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("--aubridge-update-helper") => {
            if args.len() != 4 {
                return Err("invalid-helper-arguments".into());
            }
            gateway::app_update_helper::run(
                std::path::Path::new(&args[2]),
                args[3].to_str().ok_or("invalid-helper-binding")?,
            )?;
            Ok(true)
        }
        Some("--aubridge-update-launch") => {
            if args.len() != 3 {
                return Err("invalid-launch-arguments".into());
            }
            gateway::app_update_helper::validate_owned_launch(std::path::Path::new(&args[2]))?;
            Ok(false)
        }
        _ => {
            // Development cargo binaries are not installed app bundles.
            let Ok(app) = gateway::app_update::installed_app() else {
                return Ok(false);
            };
            gateway::app_update_helper::recover_at_startup(&app)
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn dispatch() -> Result<bool, String> {
    Ok(false)
}
