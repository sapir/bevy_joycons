use bevy::prelude::*;
use bevy_joycons::JoyconsPlugin;
use rand::prelude::*;

const SQUARE_SIZE: f32 = 64.0;
const AXIS_RANGE: f32 = 128.0;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugin(JoyconsPlugin)
        .add_startup_system(setup)
        .add_system(spawn_squares_for_gamepads)
        .add_system(update_squares)
        .add_system(bevy::window::close_on_esc)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2dBundle::default());
}

#[derive(Component)]
struct ControlledByGamepad(Gamepad);

fn spawn_squares_for_gamepads(
    mut commands: Commands,
    windows: Res<Windows>,
    mut gamepad_events: EventReader<GamepadEvent>,
) {
    let primary_window = windows.primary();

    for event in gamepad_events.iter() {
        if let GamepadEventType::Connected(_) = event.event_type {
            // TODO: check if joycon
            info!("{:?} connected", event.gamepad);

            let window_size = Vec2::new(primary_window.width(), primary_window.height());
            let center = window_size / 2.0;
            let margin = SQUARE_SIZE / 2.0 + AXIS_RANGE;
            let mut rng = thread_rng();
            let x = rng.gen_range((-center.x + margin)..(center.x - margin));
            let y = rng.gen_range((-center.y + margin)..(center.y - margin));

            commands
                .spawn(SpatialBundle::from_transform(Transform::from_xyz(
                    x, y, 0.0,
                )))
                .with_children(|parent| {
                    parent.spawn((
                        SpriteBundle {
                            sprite: Sprite {
                                custom_size: Some(Vec2::splat(SQUARE_SIZE)),
                                ..default()
                            },
                            ..default()
                        },
                        ControlledByGamepad(event.gamepad),
                    ));
                });
        }
    }
}

fn update_squares(
    mut commands: Commands,
    gamepads: Res<Gamepads>,
    axes: Res<Axis<GamepadAxis>>,
    mut query: Query<(Entity, &ControlledByGamepad, &mut Transform)>,
) {
    for (entity, controlled, mut transform) in &mut query {
        let gamepad = controlled.0;

        if !gamepads.contains(gamepad) {
            info!("{:?} disconnected", gamepad);
            commands.entity(entity).despawn_recursive();
            continue;
        }

        let [x, y] = [GamepadAxisType::LeftStickX, GamepadAxisType::LeftStickY]
            .map(|axis_type| axes.get(GamepadAxis { gamepad, axis_type }).unwrap());

        let stick_pos = Vec2::new(x, y);
        transform.translation = (stick_pos * AXIS_RANGE).extend(0.0);
    }
}
