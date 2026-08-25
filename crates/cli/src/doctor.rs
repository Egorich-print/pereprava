//! `doctor` — diagnose why a phone may not be talking to us.

use anyhow::Result;
use pereprava_core::actor;

/// Runs every diagnostic and prints a report.
pub async fn run() -> Result<()> {
    println!("pereprava doctor");
    println!("================");

    // 1. USB-level enumeration (no session opened yet).
    let probes = actor::probe_devices();
    if probes.is_empty() {
        println!("\n[1] USB: no MTP devices visible.");
        println!("    - unlock the phone and set USB mode to \"File transfer / MTP\"");
        println!("    - try another cable (must be a data cable, not charge-only)");
        println!("    - try another port; avoid hubs");
    } else {
        println!("\n[1] USB: found {} MTP candidate(s):", probes.len());
        for d in &probes {
            let serial = d.serial.as_deref().unwrap_or("-");
            println!("    - {} serial={serial}", d.display());
            if let Some(sp) = &d.speed
                && sp.starts_with("High")
            {
                println!("      note: link is USB 2.0 High speed (~40 MiB/s ceiling)");
            }
        }
    }

    // 2. Session-level open (catches ptpcamerad / Android File Transfer grabs).
    println!("\n[2] Opening an MTP session...");
    match pereprava_core::DeviceHandle::connect_first().await {
        Ok(dev) => {
            let s = dev.info().await?;
            let model = s.product.as_deref().unwrap_or("unknown model");
            println!("    OK: session established with {model}");
            for st in &s.storages {
                println!(
                    "    storage: {} ({} free)",
                    st.description,
                    crate::format::human_bytes(st.free)
                );
            }
            // Always close cleanly so the phone's MTP server is not left wedged.
            if let Err(e) = dev.close().await {
                println!("    note: session close reported {e}");
            }
        }
        Err(e) => {
            let msg = e.to_string();
            println!("    FAILED: {msg}");
            let low = msg.to_ascii_lowercase();
            if low.contains("exclusive")
                || low.contains("busy")
                || low.contains("access")
                || low.contains("claim")
            {
                println!("\n    This usually means another process grabbed the device:");
                println!("      * macOS `ptpcamerad` daemon — run while transferring:");
                println!("          while true; do pkill -9 ptpcamerad 2>/dev/null; sleep 1; done");
                println!("      * Android File Transfer app — quit it (it is abandoned anyway)");
            }
        }
    }

    // 3. ADB availability (optional fast lane in later versions).
    println!("\n[3] ADB transport probe (optional):");
    match tokio::process::Command::new("adb")
        .args(["devices"])
        .output()
        .await
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let authorized = text
                .lines()
                .filter(|l| !l.starts_with("List of") && !l.trim().is_empty())
                .filter(|l| l.ends_with("device"))
                .count();
            if authorized > 0 {
                println!("    adb present, {authorized} authorized device(s).");
                println!("    Future versions can use ADB as an accelerated zstd lane.");
            } else {
                println!("    adb present but no authorized device (that's fine).");
            }
        }
        Err(_) => println!("    adb binary not found (fine; MTP works without it)."),
    }

    Ok(())
}
