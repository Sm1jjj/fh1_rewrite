use super::{SolarikLighting, prepare::SolarikLightingResources};
use bevy_camera::Camera;
use bevy_ecs::system::{Commands, ResMut};
use bevy_pbr::deferred::SkipDeferredLighting;
use bevy_render::{MainWorld, sync_world::RenderEntity};

pub fn extract_solari_lighting(mut main_world: ResMut<MainWorld>, mut commands: Commands) {
    let mut cameras_3d =
        main_world.query::<(RenderEntity, &Camera, Option<&mut SolarikLighting>)>();

    for (entity, camera, solarik_lighting) in cameras_3d.iter_mut(&mut main_world) {
        let mut entity_commands = commands
            .get_entity(entity)
            .expect("Camera entity wasn't synced.");
        if let Some(mut solarik_lighting) = solarik_lighting
            && camera.is_active
        {
            entity_commands.insert((solarik_lighting.clone(), SkipDeferredLighting));
            solarik_lighting.reset = false;
        } else {
            entity_commands.remove::<(
                SolarikLighting,
                SolarikLightingResources,
                SkipDeferredLighting,
            )>();
        }
    }
}
