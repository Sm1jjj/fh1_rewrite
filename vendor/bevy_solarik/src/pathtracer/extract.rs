use super::{Pathtracer, prepare::PathtracerAccumulationTexture};
use bevy_camera::Camera;
use bevy_ecs::{
    entity::EntityHashMap,
    system::{Commands, Local, Query},
};
use bevy_render::{Extract, sync_world::RenderEntity};
use bevy_transform::components::GlobalTransform;

pub fn extract_pathtracer(
    cameras_3d: Extract<Query<(RenderEntity, &Camera, &GlobalTransform, Option<&Pathtracer>)>>,
    mut previous_transforms: Local<EntityHashMap<GlobalTransform>>,
    mut commands: Commands,
) {
    for (entity, camera, global_transform, pathtracer) in &cameras_3d {
        let mut entity_commands = commands
            .get_entity(entity)
            .expect("Camera entity wasn't synced.");
        if let Some(pathtracer) = pathtracer
            && camera.is_active
        {
            let mut pathtracer = pathtracer.clone();
            // Compared by value rather than by change tick: read from this
            // extract system, `Ref<GlobalTransform>::is_changed()` reported a
            // camera that had not moved as changed every frame, so the
            // accumulation never got past one sample.
            let moved =
                previous_transforms.insert(entity, *global_transform) != Some(*global_transform);
            pathtracer.reset |= moved;
            entity_commands.insert(pathtracer);
        } else {
            previous_transforms.remove(&entity);
            entity_commands.remove::<(Pathtracer, PathtracerAccumulationTexture)>();
        }
    }
}
