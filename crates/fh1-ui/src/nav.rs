//! `colorado.nav`: the open-world road network the minimap and world map draw (and the satnav
//! routes on). Big-endian, OpenStreetMap-like: nodes, ways (polylines) and key/value tags shared by
//! both. Coordinates are collision space (left-handed, +Z north): negate Z for the engine.
//! VERIFIED: every byte accounted for; roads sit 1.85 m (median) from the 7,867 TrackRoute starts.
//!
//! Layout: header `10 × u32` (magic 0x0E177551, n_nodes, n_ways, n_tags, n_way_nodes,
//! n_node_ways, n_keys, n_vals, key_bytes, val_bytes); nodes `32 ×` {u32 id, f32 x, y, z,
//! u32 n_ways, way_ref_start, n_tags, tag_start}; ways `16 ×` {u32 n_nodes, node_ref_start, n_tags,
//! tag_start}; way node indices `u32`; node→way back refs `2 × u32`; tags `2 × u32` (key, value);
//! key and value offsets; NUL-terminated key then value strings; the magic again.

use std::collections::HashMap;

use crate::reader::Reader;
use crate::{need, Result};

const MAGIC: u32 = 0x0E17_7551;

#[derive(Debug, Clone)]
pub struct Node {
    pub id: u32,
    /// Collision space, metres.
    pub pos: [f32; 3],
    pub tags: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Way {
    /// Indices into [`Nav::nodes`], in order.
    pub nodes: Vec<u32>,
    pub tags: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Nav {
    pub nodes: Vec<Node>,
    pub ways: Vec<Way>,
}

impl Nav {
    pub fn parse(d: &[u8]) -> Result<Self> {
        let mut r = Reader::new(d, 0);
        let h: Vec<u32> = (0..10).map(|_| r.u32("nav")).collect::<Result<_>>()?;
        need(h[0] == MAGIC, || format!("nav magic {:08x}", h[0]))?;
        let [_, n_nodes, n_ways, n_tags, n_way_nodes, n_node_ways, n_keys, n_vals, key_bytes, val_bytes] =
            h[..].try_into().map_err(|_| crate::Error::Format("nav header".into()))?;
        let mut raw_nodes = Vec::with_capacity(n_nodes as usize);
        for _ in 0..n_nodes {
            let id = r.u32("nav")?;
            let pos = [r.f32("nav")?, r.f32("nav")?, r.f32("nav")?];
            let (_nw, _ws, nt, ts) = (r.u32("nav")?, r.u32("nav")?, r.u32("nav")?, r.u32("nav")?);
            raw_nodes.push((id, pos, nt, ts));
        }
        let mut raw_ways = Vec::with_capacity(n_ways as usize);
        for _ in 0..n_ways {
            raw_ways.push((r.u32("nav")?, r.u32("nav")?, r.u32("nav")?, r.u32("nav")?));
        }
        let way_nodes: Vec<u32> = (0..n_way_nodes).map(|_| r.u32("nav")).collect::<Result<_>>()?;
        r.bytes(8 * n_node_ways as usize, "nav node-way refs")?;
        let tags: Vec<(u32, u32)> = (0..n_tags).map(|_| Ok((r.u32("nav")?, r.u32("nav")?))).collect::<Result<_>>()?;
        let koff: Vec<u32> = (0..n_keys).map(|_| r.u32("nav")).collect::<Result<_>>()?;
        let voff: Vec<u32> = (0..n_vals).map(|_| r.u32("nav")).collect::<Result<_>>()?;
        let kb = r.o;
        let vb = kb + key_bytes as usize;
        need(vb + val_bytes as usize + 4 == d.len(), || "nav: sizes don't add up to the file length".into())?;
        need(d[d.len() - 4..] == d[..4], || "nav: no trailing magic".into())?;
        let cstr = |at: usize| -> Result<String> {
            let s = d.get(at..).ok_or_else(|| crate::Error::Format("nav string".into()))?;
            let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
            Ok(s[..end].iter().map(|&c| c as char).collect())
        };
        let keys: Vec<String> = koff.iter().map(|&o| cstr(kb + o as usize)).collect::<Result<_>>()?;
        let vals: Vec<String> = voff.iter().map(|&o| cstr(vb + o as usize)).collect::<Result<_>>()?;
        let tagmap = |start: u32, n: u32| -> HashMap<String, String> {
            tags.get(start as usize..(start + n) as usize)
                .unwrap_or(&[])
                .iter()
                .filter_map(|&(k, v)| Some((keys.get(k as usize)?.clone(), vals.get(v as usize)?.clone())))
                .collect()
        };
        let nodes = raw_nodes.into_iter().map(|(id, pos, nt, ts)| Node { id, pos, tags: tagmap(ts, nt) }).collect();
        let ways = raw_ways
            .into_iter()
            .map(|(nn, ns, nt, ts)| Way { nodes: way_nodes.get(ns as usize..(ns + nn) as usize).unwrap_or(&[]).to_vec(), tags: tagmap(ts, nt) })
            .collect();
        Ok(Self { nodes, ways })
    }

    /// The satnav graph: every way with a `road_type` (the drawable ones plus satnav-only `none` /
    /// `shortcut`), linking consecutive nodes. `oneway=true` ways run in node order only (GUESS
    /// direction). Edge weights are metres in the ground plane.
    pub fn graph(&self) -> Graph {
        let mut adj = vec![Vec::new(); self.nodes.len()];
        for w in self.ways.iter().filter(|w| w.tags.contains_key("road_type")) {
            let oneway = w.tags.get("oneway").is_some_and(|v| v == "true");
            for p in w.nodes.windows(2) {
                let (a, b) = (p[0] as usize, p[1] as usize);
                let (Some(na), Some(nb)) = (self.nodes.get(a), self.nodes.get(b)) else { continue };
                let d = ((na.pos[0] - nb.pos[0]).powi(2) + (na.pos[2] - nb.pos[2]).powi(2)).sqrt();
                adj[a].push((b as u32, d));
                if !oneway {
                    adj[b].push((a as u32, d));
                }
            }
        }
        Graph { pos: self.nodes.iter().map(|n| [n.pos[0], -n.pos[2]]).collect(), adj }
    }

    /// Drawable roads (`road_type` dirt / b / a / freeway; `none` and `shortcut` are satnav-only)
    /// as engine-space `[x, z]` polylines (collision Z negated).
    pub fn roads(&self) -> Vec<(String, Vec<[f32; 2]>)> {
        self.ways
            .iter()
            .filter_map(|w| {
                let t = w.tags.get("road_type")?;
                matches!(t.as_str(), "dirt" | "b" | "a" | "freeway").then(|| {
                    let pts = w.nodes.iter().filter_map(|&i| self.nodes.get(i as usize)).map(|n| [n.pos[0], -n.pos[2]]).collect();
                    (t.clone(), pts)
                })
            })
            .collect()
    }
}

/// The satnav road graph ([`Nav::graph`]), in engine space `[x, z]` (collision Z negated).
#[derive(Debug, Clone)]
pub struct Graph {
    pub pos: Vec<[f32; 2]>,
    /// Outgoing (node, metres) per node.
    pub adj: Vec<Vec<(u32, f32)>>,
}

impl Graph {
    /// The connected node nearest to `p`.
    pub fn nearest(&self, p: [f32; 2]) -> Option<u32> {
        let d2 = |q: &[f32; 2]| (q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2);
        (0..self.pos.len()).filter(|&i| !self.adj[i].is_empty()).min_by(|&a, &b| d2(&self.pos[a]).total_cmp(&d2(&self.pos[b]))).map(|i| i as u32)
    }

    /// Shortest road path (A*) from the node nearest `from` to the node nearest `to`: the polyline
    /// (starting at `from`, ending at `to`) and its length in metres. How the game weights roads
    /// (road type, traffic) is unknown: plain distance (GUESS).
    pub fn route(&self, from: [f32; 2], to: [f32; 2]) -> Option<(Vec<[f32; 2]>, f32)> {
        let nodes = self.route_nodes(from, to)?;
        let mut pts = vec![from];
        pts.extend(nodes.iter().map(|&i| self.pos[i as usize]));
        pts.push(to);
        let len = pts.windows(2).map(|w| ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt()).sum();
        Some((pts, len))
    }

    /// The node path of [`Graph::route`] (first = nearest `from`, last = nearest `to`), for callers that need the
    /// junctions (`adj` degree) or node heights.
    pub fn route_nodes(&self, from: [f32; 2], to: [f32; 2]) -> Option<Vec<u32>> {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let (s, g) = (self.nearest(from)?, self.nearest(to)?);
        let h = |i: u32| {
            let (a, b) = (self.pos[i as usize], self.pos[g as usize]);
            ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
        };
        let mut dist = vec![f32::INFINITY; self.pos.len()];
        let mut prev = vec![u32::MAX; self.pos.len()];
        let mut heap = BinaryHeap::new();
        dist[s as usize] = 0.0;
        heap.push(Reverse(((h(s) * 16.0) as u64, s)));
        while let Some(Reverse((_, u))) = heap.pop() {
            if u == g {
                break;
            }
            for &(v, w) in &self.adj[u as usize] {
                let nd = dist[u as usize] + w;
                if nd < dist[v as usize] {
                    dist[v as usize] = nd;
                    prev[v as usize] = u;
                    heap.push(Reverse((((nd + h(v)) * 16.0) as u64, v)));
                }
            }
        }
        if !dist[g as usize].is_finite() {
            return None;
        }
        let mut nodes = vec![g];
        while *nodes.last()? != s {
            nodes.push(prev[*nodes.last()? as usize]);
        }
        nodes.reverse();
        Some(nodes)
    }
}
