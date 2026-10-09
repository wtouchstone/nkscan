use anyhow::{Result, anyhow};
use nkscan::{device, session::Session};

use crate::cli::Calibrate;

/// Nikon Scan's Calibrate: the scanner measures its light source again
pub async fn run(args: Calibrate) -> Result<()> {
    let selector = match args.device {
        Some(d) => device::Selector::Location(d),
        None => device::Selector::Only,
    };
    let (device, transport) = match selector.open().await {
        Ok(opened) => opened,
        Err(e) => {
            let list: Vec<_> = device::list()
                .await
                .iter()
                .map(ToString::to_string)
                .collect();
            return Err(anyhow!("{e}\n\nattached:\n  {}", list.join("\n  ")));
        }
    };

    println!("{device}");

    let mut session = Session::open(transport).await?;

    println!("Connected to scanner");

    if session.calibrate_shading().await? {
        // The measurement moves the mechanism, so put the film back
        session.stage().await?;
        println!("Calibrated");
    } else {
        println!("Scanner does not support calibration");
    }

    Ok(())
}
