use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread::spawn,
};

use anyhow::{Context, Result};
use bevy_app::{App, CoreStage, Plugin};
use bevy_ecs::{event::EventWriter, schedule::IntoSystemDescriptor, system::NonSendMut};
use bevy_input::{
    gamepad::{Gamepad, GamepadAxisType, GamepadEventRaw, GamepadEventType, GamepadInfo},
    InputSystem,
};
use bevy_utils::{
    tracing::{error, info},
    HashMap,
};
use joycon::{
    hidapi::{DeviceInfo, HidApi},
    joycon_sys::{input::WhichController, HID_IDS, NINTENDO_VENDOR_ID},
    JoyCon as JoyconDevice, Report as JoyconReport,
};
use pinboard::Pinboard;
use thunderdome::{Arena, Index};

// We start at a really high number to avoid conflicting with gilrs.
const STARTING_GAMEPAD_ID: usize = 0x8000_0000;

#[derive(Default)]
pub struct JoyconsPlugin;

impl Plugin for JoyconsPlugin {
    fn build(&self, app: &mut App) {
        let hidapi = match HidApi::new_without_enumerate() {
            Ok(x) => x,
            Err(e) => {
                error!("Failed to setup HidApi: {}", e);
                return;
            }
        };

        app.insert_non_send_resource(Joycons::new(hidapi))
            .add_system_to_stage(
                CoreStage::PreUpdate,
                detect_connection_changes.before(InputSystem),
            )
            .add_system_to_stage(
                CoreStage::PreUpdate,
                update_joycon_data
                    .after(detect_connection_changes)
                    .before(InputSystem),
            );
    }
}

struct Joycons {
    hidapi: HidApi,
    joycons_by_serial_number: HashMap<String, Result<Joycon, ()>>,
    trackers: Arena<Tracker>,
    next_gamepad_id: AtomicUsize,
}

impl Joycons {
    fn new(hidapi: HidApi) -> Self {
        Self {
            hidapi,
            joycons_by_serial_number: HashMap::new(),
            trackers: Arena::new(),
            next_gamepad_id: AtomicUsize::new(STARTING_GAMEPAD_ID),
        }
    }
}

fn detect_connection_changes(
    mut joycons: NonSendMut<Joycons>,
    mut events: EventWriter<GamepadEventRaw>,
) {
    if let Err(e) = detect_connection_changes_inner(&mut joycons, &mut events) {
        error!("Error detecting joycon connections/disconnections: {}", e);
    }
}

fn detect_connection_changes_inner(
    joycons: &mut Joycons,
    events: &mut EventWriter<GamepadEventRaw>,
) -> Result<()> {
    joycons
        .hidapi
        .refresh_devices()
        .context("Refreshing hidapi device list")?;

    for device_info in joycons.hidapi.device_list() {
        if !is_joycon_device(device_info) {
            continue;
        }

        let Some(serial_num) = device_info.serial_number() else {
            error!("Bad joycon serial number");
            continue;
        };

        if joycons.joycons_by_serial_number.contains_key(serial_num) {
            continue;
        }

        let Some(product_string) = device_info.product_string() else {
            error!("Bad product string for joycon {}", serial_num);
            continue;
        };

        let gamepad = Gamepad {
            id: joycons.next_gamepad_id.fetch_add(1, Ordering::SeqCst),
        };
        let joycon = match Tracker::new(&joycons.hidapi, device_info, gamepad) {
            Ok((joycon_device, tracker)) => {
                info!("'{}' ({}) connected", product_string, serial_num);

                events.send(GamepadEventRaw {
                    gamepad,
                    event_type: GamepadEventType::Connected(GamepadInfo {
                        name: product_string.to_string(),
                    }),
                });

                // This needs a dedicated thread, otherwise we get (more?)
                // latency.
                spawn({
                    let product_string = tracker.product_string.clone();
                    let serial_number = tracker.serial_number.clone();
                    let last_report = tracker.last_report.clone();

                    move || {
                        joycon_polling_thread(
                            joycon_device,
                            product_string,
                            serial_number,
                            last_report,
                        );
                    }
                });

                let index = joycons.trackers.insert(tracker);
                Ok(Joycon(index))
            }

            Err(e) => {
                error!("Error opening '{}' ({}): {}", product_string, serial_num, e);
                // Remember that we had an error, so that we don't retry every
                // frame.
                Err(())
            }
        };

        joycons
            .joycons_by_serial_number
            .insert(serial_num.to_string(), joycon);
    }

    Ok(())
}

fn is_joycon_device(device_info: &DeviceInfo) -> bool {
    device_info.vendor_id() == NINTENDO_VENDOR_ID && HID_IDS.contains(&device_info.product_id())
}

#[derive(Clone, Copy, Debug)]
pub struct Joycon(Index);

struct Tracker {
    product_string: String,
    serial_number: String,
    which: WhichController,
    /// If the pinboard is empty, then the joycon thread has hit an error.
    last_report: Arc<Pinboard<JoyconReport>>,
    gamepad: Gamepad,
}

impl Tracker {
    fn new(
        hidapi: &HidApi,
        device_info: &DeviceInfo,
        gamepad: Gamepad,
    ) -> Result<(JoyconDevice, Self)> {
        let product_string = device_info.product_string().unwrap().to_string();
        let serial_number = device_info.serial_number().unwrap().to_string();

        let device = device_info
            .open_device(hidapi)
            .context("Opening joycon hid device")?;
        let mut inner =
            JoyconDevice::new(device, device_info.clone()).context("Initializing joycon")?;

        let joycon_dev_info = inner.get_dev_info().context("Getting joycon device info")?;
        let which = joycon_dev_info
            .which_controller
            .try_into()
            .context("Parsing joycon type")?;

        inner
            .load_calibration()
            .context("Loading calibration data")?;

        let report = inner.tick().context("Polling joycon first time")?;
        let last_report = Arc::new(Pinboard::new(report));

        Ok((
            inner,
            Self {
                product_string,
                serial_number,
                which,
                last_report,
                gamepad,
            },
        ))
    }
}

fn update_joycon_data(mut joycons: NonSendMut<Joycons>, mut events: EventWriter<GamepadEventRaw>) {
    for (_, wrapper) in &mut joycons.trackers {
        // TODO: identify and remove disconnected joycons
        let Some(report) = wrapper.last_report.read() else { continue };

        match wrapper.which {
            WhichController::LeftJoyCon => {
                // Rotate data by 90 degrees.
                send_axis_event(
                    &mut events,
                    wrapper.gamepad,
                    GamepadAxisType::LeftStickX,
                    -report.left_stick.y,
                    GamepadAxisType::LeftStickY,
                    report.left_stick.x,
                );
            }

            WhichController::RightJoyCon => {
                // Treat the single stick as the left stick even though it's the
                // right joycon. Also, rotate it by 90 degrees.
                send_axis_event(
                    &mut events,
                    wrapper.gamepad,
                    GamepadAxisType::LeftStickX,
                    report.right_stick.y,
                    GamepadAxisType::LeftStickY,
                    -report.right_stick.x,
                );
            }

            WhichController::ProController => {
                send_axis_event(
                    &mut events,
                    wrapper.gamepad,
                    GamepadAxisType::LeftStickX,
                    report.left_stick.x,
                    GamepadAxisType::LeftStickY,
                    report.left_stick.y,
                );
                send_axis_event(
                    &mut events,
                    wrapper.gamepad,
                    GamepadAxisType::RightStickX,
                    report.right_stick.x,
                    GamepadAxisType::RightStickY,
                    report.right_stick.y,
                );
            }
        }
    }
}

fn send_axis_event(
    events: &mut EventWriter<GamepadEventRaw>,
    gamepad: Gamepad,
    x_axis: GamepadAxisType,
    x: f64,
    y_axis: GamepadAxisType,
    y: f64,
) {
    events.send(GamepadEventRaw::new(
        gamepad,
        GamepadEventType::AxisChanged(x_axis, x as f32),
    ));
    events.send(GamepadEventRaw::new(
        gamepad,
        GamepadEventType::AxisChanged(y_axis, y as f32),
    ));
}

fn joycon_polling_thread(
    mut joycon_device: JoyconDevice,
    product_string: String,
    serial_number: String,
    last_report: Arc<Pinboard<JoyconReport>>,
) {
    loop {
        let report = match joycon_device.tick() {
            Ok(x) => x,
            Err(e) => {
                error!(
                    "Error updating '{}' ({}): {}",
                    product_string, serial_number, e
                );
                last_report.clear();
                break;
            }
        };

        last_report.set(report);
    }
}
