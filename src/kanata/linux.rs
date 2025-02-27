use anyhow::{anyhow, bail, Result};
use evdev::{InputEvent, InputEventKind, RelativeAxisType};
use log::info;
use parking_lot::Mutex;
use std::convert::TryFrom;
use std::sync::mpsc::SyncSender as Sender;
use std::sync::Arc;

use super::*;

impl Kanata {
    /// Enter an infinite loop that listens for OS key events and sends them to the processing
    /// thread.
    pub fn event_loop(kanata: Arc<Mutex<Self>>, tx: Sender<KeyEvent>) -> Result<()> {
        info!("entering the event loop");

        let k = kanata.lock();
        let mut kbd_in = match KbdIn::new(
            &k.kbd_in_paths,
            k.continue_if_no_devices,
            k.include_names.clone(),
            k.exclude_names.clone(),
        ) {
            Ok(kbd_in) => kbd_in,
            Err(e) => {
                bail!("failed to open keyboard device(s): {}", e)
            }
        };

        // In some environments, this needs to be done after the input device grab otherwise it
        // does not work on kanata startup.
        Kanata::set_repeat_rate(&k.defcfg_items)?;
        drop(k);

        loop {
            let events = kbd_in.read().map_err(|e| anyhow!("failed read: {}", e))?;
            log::trace!("{events:?}");

            for in_event in events.iter().copied() {
                let key_event = match KeyEvent::try_from(in_event) {
                    Ok(ev) => ev,
                    _ => {
                        // Pass-through non-key and non-scroll events
                        let mut kanata = kanata.lock();
                        kanata
                            .kbd_out
                            .write_raw(in_event)
                            .map_err(|e| anyhow!("failed write: {}", e))?;
                        continue;
                    }
                };

                check_for_exit(&key_event);

                if key_event.value == KeyValue::Tap {
                    // Scroll event for sure. Only scroll events produce Tap.
                    if !handle_scroll(&kanata, in_event, key_event.code, &events)? {
                        continue;
                    }
                } else {
                    // Handle normal keypresses.
                    // Check if this keycode is mapped in the configuration.
                    // If it hasn't been mapped, send it immediately.
                    if !MAPPED_KEYS.lock().contains(&key_event.code) {
                        let mut kanata = kanata.lock();
                        kanata
                            .kbd_out
                            .write_raw(in_event)
                            .map_err(|e| anyhow!("failed write: {}", e))?;
                        continue;
                    };
                }

                // Send key events to the processing loop
                if let Err(e) = tx.try_send(key_event) {
                    bail!("failed to send on channel: {}", e)
                }
            }
        }
    }

    pub fn check_release_non_physical_shift(&mut self) -> Result<()> {
        Ok(())
    }

    pub fn set_repeat_rate(cfg_items: &HashMap<String, String>) -> Result<()> {
        if let Some(x11_rpt_str) = cfg_items.get("linux-x11-repeat-delay-rate") {
            let delay_rate = x11_rpt_str.split(',').collect::<Vec<_>>();
            let errmsg = format!("Invalid value for linux-x11-repeat-delay-rate: \"{x11_rpt_str}\".\nExpected two numbers 0-65535 separated by a comma, e.g. 200,25");
            if delay_rate.len() != 2 {
                log::error!("{errmsg}");
            }
            str::parse::<u16>(delay_rate[0]).map_err(|e| {
                log::error!("{errmsg}");
                e
            })?;
            str::parse::<u16>(delay_rate[1]).map_err(|e| {
                log::error!("{errmsg}");
                e
            })?;
            log::info!(
                "Using xset to set X11 repeat delay to {} and repeat rate to {}",
                delay_rate[0],
                delay_rate[1]
            );
            let cmd_output = std::process::Command::new("xset")
                .args(["r", "rate", delay_rate[0], delay_rate[1]])
                .output()
                .map_err(|e| {
                    log::error!("failed to run xset: {e:?}");
                    e
                })?;
            log::info!(
                "xset stdout: {}",
                String::from_utf8_lossy(&cmd_output.stdout)
            );
            log::info!(
                "xset stderr: {}",
                String::from_utf8_lossy(&cmd_output.stderr)
            );
        }
        Ok(())
    }
}

/// Returns true if the scroll event should be sent to the processing loop, otherwise returns
/// false.
fn handle_scroll(
    kanata: &Mutex<Kanata>,
    in_event: InputEvent,
    code: OsCode,
    all_events: &[InputEvent],
) -> Result<bool> {
    let direction: MWheelDirection = code.try_into().unwrap();
    let scroll_distance = in_event.value().unsigned_abs() as u16;
    match in_event.kind() {
        InputEventKind::RelAxis(axis_type) => {
            match axis_type {
                RelativeAxisType::REL_WHEEL | RelativeAxisType::REL_HWHEEL => {
                    if MAPPED_KEYS.lock().contains(&code) {
                        return Ok(true);
                    }
                    // If we just used `write_raw` here, some of the scrolls issued by kanata would be
                    // REL_WHEEL_HI_RES + REL_WHEEL and some just REL_WHEEL and an issue like this one
                    // would happen: https://github.com/jtroo/kanata/issues/395
                    //
                    // So to fix this case, we need to use `scroll` which will also send hi-res scrolls
                    // along normal scrolls.
                    //
                    // However, if this is a normal scroll event, it may be sent alongside a hi-res
                    // scroll event. In this scenario, the hi-res event should be used to call
                    // scroll, and not the normal event. Otherwise, too much scrolling will happen.
                    let mut kanata = kanata.lock();
                    if !all_events.iter().any(|ev| {
                        matches!(
                            ev.kind(),
                            InputEventKind::RelAxis(
                                RelativeAxisType::REL_WHEEL_HI_RES
                                    | RelativeAxisType::REL_HWHEEL_HI_RES
                            )
                        )
                    }) {
                        kanata
                            .kbd_out
                            .scroll(direction, scroll_distance * HI_RES_SCROLL_UNITS_IN_LO_RES)
                            .map_err(|e| anyhow!("failed write: {}", e))?;
                    }
                    Ok(false)
                }
                RelativeAxisType::REL_WHEEL_HI_RES | RelativeAxisType::REL_HWHEEL_HI_RES => {
                    if !MAPPED_KEYS.lock().contains(&code) {
                        // Passthrough if the scroll wheel event is not mapped
                        // in the configuration.
                        let mut kanata = kanata.lock();
                        kanata
                            .kbd_out
                            .scroll(direction, scroll_distance)
                            .map_err(|e| anyhow!("failed write: {}", e))?;
                    }
                    // Kanata will not handle high resolution scroll events for now.
                    // Full notch scrolling only.
                    Ok(false)
                }
                _ => unreachable!("expect to be handling a wheel event"),
            }
        }
        _ => unreachable!("expect to be handling a wheel event"),
    }
}
