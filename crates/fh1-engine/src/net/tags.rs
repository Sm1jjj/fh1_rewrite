//! Name tags over other players' cars (P18, docs/MULTIPLAYER.md "Presence"): a screen-space label per [`RemoteCar`],
//! anchored above its roof through the game camera, in the HUD font. Full size and opacity near, smaller and fading
//! out to [`FAR`]; hidden behind the camera, beyond [`FAR`], with the HUD off, or when the world (terrain / scenery
//! collision) is between the camera and the car, and while the pause menu is open. Never on your own car (it is not a `RemoteCar`). `FH1_NET_TAGS=0` =
//! none.

use std::collections::HashMap;

use bevy::prelude::*;

use super::RemoteCar;
use crate::track::Track;

/// Full opacity up to here (m), gone at [`FAR`].
const NEAR: f32 = 60.0;
const FAR: f32 = 180.0;
/// Font size near (px) and at [`FAR`].
const PX_NEAR: f32 = 20.0;
const PX_FAR: f32 = 13.0;
/// Above the car's roof (m).
const LIFT: f32 = 0.7;
/// Occlusion ray per tag at most this often (s).
const RAY_EVERY: f32 = 0.12;
/// Opacity change per second (fade in / out, occlusion).
const FADE_RATE: f32 = 6.0;

fn tags_on() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("FH1_NET_TAGS").map_or(true, |v| v != "0"))
}

pub(super) fn plugin(app: &mut App) {
    if tags_on() {
        app.add_systems(Update, update_tags.after(super::puppets_posed));
    }
}

/// A tag's UI node; `car` = its [`RemoteCar`].
#[derive(Component)]
struct NameTag {
    car: Entity,
    alpha: f32,
    occluded: bool,
    next_ray: f32,
    px: f32,
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_tags(
    mut commands: Commands,
    font: Option<Res<crate::ui::UiFont>>,
    settings: Res<crate::ui::Settings>,
    menu: Option<Res<crate::ui::Menu>>,
    track: Res<Track>,
    time: Res<Time<Real>>,
    cams: Query<(&Camera, &GlobalTransform), With<fh1_render::post::FxPostCamera>>,
    cars: Query<(Entity, &RemoteCar, &Visibility)>,
    mut tags: Query<(Entity, &mut NameTag, &mut Node, &mut TextColor, &mut TextShadow, &mut TextFont, &mut Visibility), Without<RemoteCar>>,
    mut by_car: Local<HashMap<Entity, Entity>>,
) {
    let dt = time.delta_secs().min(0.1);
    let now = time.elapsed_secs();
    let cam = cams.iter().find(|(c, _)| c.is_active);
    // Tags whose car is gone.
    for (e, tag, ..) in &tags {
        if !cars.contains(tag.car) {
            commands.entity(e).despawn();
            by_car.remove(&tag.car);
        }
    }
    // New cars get a tag (hidden until placed).
    let Some(font) = font else { return };
    for (car, remote, _) in &cars {
        if by_car.contains_key(&car) {
            continue;
        }
        let tag = commands
            .spawn((
                Node { position_type: PositionType::Absolute, ..default() },
                Text::new(remote.name()),
                font.text(PX_NEAR),
                TextColor(Color::WHITE.with_alpha(0.0)),
                TextShadow { offset: Vec2::new(1.5, 1.5), color: Color::BLACK.with_alpha(0.0) },
                TextLayout::no_wrap(),
                // Centred over the anchor, sitting on it.
                UiTransform { translation: Val2::percent(-50.0, -100.0), ..UiTransform::IDENTITY },
                Visibility::Hidden,
                NameTag { car, alpha: 0.0, occluded: false, next_ray: 0.0, px: PX_NEAR },
                Name::new("net name tag"),
            ))
            .id();
        by_car.insert(car, tag);
    }
    for (_, mut tag, mut node, mut color, mut shadow, mut text_font, mut vis) in &mut tags {
        let Ok((_, remote, car_vis)) = cars.get(tag.car) else { continue };
        let v = &remote.vehicle;
        // The roof: the body box top over the centre of mass (world up, so a rolled car keeps its tag above it).
        let head = v.position + Vec3::Y * (v.data.bbox[1].y - v.cg_model.y + LIFT).max(1.0);
        let mut target = 0.0;
        let mut screen = None;
        if let Some((camera, cam_t)) = cam {
            let eye = cam_t.translation();
            let dist = eye.distance(head);
            // Not over the pause menu.
            let menu_open = menu.as_ref().is_some_and(|m| m.open);
            if settings.hud && !menu_open && *car_vis != Visibility::Hidden && dist < FAR {
                if let Ok(p) = camera.world_to_viewport(cam_t, head) {
                    screen = Some((p, dist));
                    // Occlusion by the world, checked a few times a second (one ray per tag).
                    if now >= tag.next_ray {
                        tag.next_ray = now + RAY_EVERY;
                        let dir = (head - eye) / dist.max(1e-3);
                        tag.occluded = track.ground.ray(eye, dir, dist).is_some_and(|h| h.distance < dist - 1.5);
                    }
                    if !tag.occluded {
                        target = (1.0 - (dist - NEAR) / (FAR - NEAR)).clamp(0.0, 1.0);
                    }
                }
            }
        }
        let step = FADE_RATE * dt;
        tag.alpha = if target > tag.alpha { (tag.alpha + step).min(target) } else { (tag.alpha - step).max(target) };
        let show = tag.alpha > 0.01 && screen.is_some();
        let want = if show { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
        let Some((p, dist)) = screen.filter(|_| show) else { continue };
        node.left = Val::Px(p.x);
        node.top = Val::Px(p.y);
        color.0 = Color::WHITE.with_alpha(tag.alpha);
        shadow.color = Color::BLACK.with_alpha(0.8 * tag.alpha);
        // Smaller with distance, in whole pixels (a font size change re-lays the text out).
        let px = (PX_NEAR - (PX_NEAR - PX_FAR) * ((dist - NEAR) / (FAR - NEAR)).clamp(0.0, 1.0)).round();
        if (px - tag.px).abs() >= 1.0 {
            tag.px = px;
            *text_font = font.text(px);
        }
    }
}
