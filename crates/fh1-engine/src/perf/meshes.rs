//! Mesh upload census for the gameplay recorder (P9, 2026-10-08: log 135653's hitches were nearly all
//! allocate_and_free_meshes 36-71 ms, also in seconds without any scenery streaming). Two render-world systems around
//! Bevy's `allocate_and_free_meshes`: before it, the meshes extracted this frame (added or modified: count, vertex +
//! index bytes, the 3 largest with their attributes and asset id) and the removed count; after it, the mesh allocator's
//! slab count and bytes, so a slab regrow shows as a jump. Accumulated until the recorder reads it each main frame
//! ([`take`]); record.rs puts it into hitch rows and the per-second `mesh_uploads` / `mesh_upload_mb` columns.

use std::sync::Mutex;

use bevy::prelude::*;
use bevy::render::mesh::allocator::{allocate_and_free_meshes, MeshAllocator};
use bevy::render::mesh::RenderMesh;
use bevy::render::render_asset::ExtractedAssets;
use bevy::render::{Render, RenderApp, RenderSystems};

/// One extracted mesh: (bytes, vertices, indices, attribute names, asset id, added (else modified)).
#[derive(Clone, Debug)]
pub(super) struct Big {
    pub bytes: u64,
    pub verts: usize,
    pub indices: usize,
    pub attrs: String,
    pub id: String,
    pub added: bool,
}

/// Mesh uploads since the recorder's last read.
#[derive(Clone, Debug, Default)]
pub(super) struct Uploads {
    pub count: u32,
    pub removed: u32,
    pub bytes: u64,
    pub top: Vec<Big>,
    /// Slab count / bytes before the first and after the last allocate in the window.
    pub slabs: Option<(usize, usize)>,
    pub slab_bytes: Option<(u64, u64)>,
}

impl Uploads {
    /// "meshes: +12 (34.5 MB) -3 removed; largest: 120k v 360k i POS|NORMAL|UV0 #AssetId..(mod); slabs 34 -> 35 (1.20 GB)".
    pub fn describe(&self) -> String {
        let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
        let mut s = format!("meshes: +{} ({:.1} MB)", self.count, mb(self.bytes));
        if self.removed > 0 {
            s += &format!(" -{} removed", self.removed);
        }
        if !self.top.is_empty() {
            let big: Vec<String> = self
                .top
                .iter()
                .map(|b| format!("{:.1} MB {}k v {}k i {} {}{}", mb(b.bytes), b.verts / 1000, b.indices / 1000, b.attrs, b.id, if b.added { "" } else { " (modified)" }))
                .collect();
            s += &format!("; largest: {}", big.join(" / "));
        }
        if let (Some((a, b)), Some((_, bytes))) = (self.slabs, self.slab_bytes) {
            s += &format!("; slabs {a} -> {b} ({:.2} GB)", bytes as f64 / (1024.0 * 1024.0 * 1024.0));
        }
        s
    }
}

static PENDING: Mutex<Option<Uploads>> = Mutex::new(None);

/// Uploads since the last call (main world, every frame).
pub(super) fn take() -> Uploads {
    PENDING.lock().ok().and_then(|mut g| g.take()).unwrap_or_default()
}

pub(super) fn plugin(app: &mut App) {
    if let Some(ra) = app.get_sub_app_mut(RenderApp) {
        ra.add_systems(Render, (before.before(allocate_and_free_meshes), after.after(allocate_and_free_meshes)).in_set(RenderSystems::PrepareAssets));
    }
}

fn mesh_bytes(m: &Mesh) -> u64 {
    let idx = m.indices().map_or(0, |i| match i {
        bevy::mesh::Indices::U16(v) => v.len() as u64 * 2,
        bevy::mesh::Indices::U32(v) => v.len() as u64 * 4,
    });
    m.count_vertices() as u64 * m.get_vertex_size() + idx
}

fn before(extracted: Option<Res<ExtractedAssets<RenderMesh>>>, alloc: Option<Res<MeshAllocator>>) {
    let Some(ex) = extracted else { return };
    let Ok(mut g) = PENDING.lock() else { return };
    let u = g.get_or_insert_with(Uploads::default);
    if u.slabs.is_none() {
        if let Some(a) = alloc.as_deref() {
            u.slabs = Some((a.slab_count(), a.slab_count()));
            u.slab_bytes = Some((a.slabs_size(), a.slabs_size()));
        }
    }
    u.removed += ex.removed.len() as u32;
    for (id, mesh) in &ex.extracted {
        let bytes = mesh_bytes(mesh);
        u.count += 1;
        u.bytes += bytes;
        if u.top.len() == 3 && u.top.last().is_some_and(|b| b.bytes >= bytes) {
            continue;
        }
        let attrs = mesh.attributes().map(|(a, _)| a.name.trim_start_matches("Vertex_")).collect::<Vec<_>>().join("|");
        let big = Big { bytes, verts: mesh.count_vertices(), indices: mesh.indices().map_or(0, |i| i.len()), attrs, id: format!("{id:?}"), added: ex.added.contains(id) };
        let at = u.top.iter().position(|b| b.bytes < bytes).unwrap_or(u.top.len());
        u.top.insert(at, big);
        u.top.truncate(3);
    }
}

fn after(alloc: Option<Res<MeshAllocator>>) {
    let Some(a) = alloc else { return };
    let Ok(mut g) = PENDING.lock() else { return };
    let u = g.get_or_insert_with(Uploads::default);
    let (n, bytes) = (a.slab_count(), a.slabs_size());
    u.slabs = Some((u.slabs.map_or(n, |s| s.0), n));
    u.slab_bytes = Some((u.slab_bytes.map_or(bytes, |s| s.0), bytes));
}
