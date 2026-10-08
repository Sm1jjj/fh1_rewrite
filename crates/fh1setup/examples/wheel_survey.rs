//! wheel_survey <disc> : per car, the stock rim model's dimensions vs gamedb tyre/rim sizes.
//! CSV on stdout. Radii are sqrt(y^2 + z^2) about the wheel axis (X), metres.
use fh1_formats::{carbin, zip::Archive};
use rusqlite::Connection;

static FIX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct Dims {
    r_min: f32,
    r_max: f32,
    x_min: f32,
    x_max: f32,
}

/// LOD0-pool positions re-mapped with the LOD pool's pack transform (pool bbox -> section bounds).
fn fix(s: &carbin::Section, p: [f32; 3]) -> [f32; 3] {
    let (a, b) = (&s.lod0_raw, &s.lod_raw);
    if b.stride == 0 || a.stride == 0 { return p; }
    let mut o = [0.0; 3];
    for k in 0..3 {
        let br = s.bounds_max[k] - s.bounds_min[k];
        let raw = a.pool_min[k] + (p[k] - s.bounds_min[k]) / br * (a.pool_max[k] - a.pool_min[k]);
        o[k] = s.bounds_min[k] + (raw - b.pool_min[k]) / (b.pool_max[k] - b.pool_min[k]) * br;
    }
    o
}

fn dims(s: &carbin::Section, pred: &dyn Fn(&carbin::Subsection) -> bool) -> Option<Dims> {
    let subs: Vec<_> = s.subsections.iter().filter(|x| pred(x)).collect();
    let lod = subs.iter().map(|x| x.lod).min()?;
    let mut d = Dims { r_min: f32::MAX, r_max: 0.0, x_min: f32::MAX, x_max: f32::MIN };
    for sub in subs.iter().filter(|x| x.lod == lod) {
        let pool = s.vertices_for(sub);
        for &i in &sub.indices {
            let mut p = pool[i as usize].position;
            if FIX.load(std::sync::atomic::Ordering::Relaxed) && sub.lod == 0 && !s.lod0_vertices.is_empty() { p = fix(s, p); }
            let (x, y, z) = (p[0] + s.offset[0], p[1] + s.offset[1], p[2] + s.offset[2]);
            let r = (y * y + z * z).sqrt();
            d.r_min = d.r_min.min(r);
            d.r_max = d.r_max.max(r);
            d.x_min = d.x_min.min(x);
            d.x_max = d.x_max.max(x);
        }
    }
    (d.r_max > 0.0).then_some(d)
}

fn read(zip: &std::path::Path, name: &str) -> Option<carbin::Carbin> {
    let mut ar = Archive::open(zip).ok()?;
    let e = ar.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned()?;
    carbin::parse(&ar.read(&e).ok()?).ok()
}

fn main() {
    let disc = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let db = Connection::open(disc.join("media/db/gamedb.slt")).unwrap();
    let mut st = db
        .prepare(
            "SELECT c.MediaName, w.MediaName, c.FrontTireWidthMM, c.FrontTireAspect, c.FrontWheelDiameterIN,
                    c.RearTireWidthMM, c.RearTireAspect, c.RearWheelDiameterIN, b.ModelFrontTrackOuter, b.ModelRearTrackOuter,
                    b.ModelWheelbase, c.FrontStockRideHeight, c.RearStockRideHeight
             FROM Data_Car c LEFT JOIN List_Wheels w ON w.ID = c.StockWheelID
             LEFT JOIN List_UpgradeCarBody u ON u.Ordinal = c.Id AND u.IsStock = 1
             LEFT JOIN Data_CarBody b ON b.Id = u.CarBodyID ORDER BY c.MediaName",
        )
        .unwrap();
    println!("car,rim,fw,fa,fd,rw,ra,rd,ftrack,rtrack,wb,frh,rrh,rim_tyre_rmin,rim_tyre_rmax,rim_tyre_w,rim_rim_rmax,rim_rim_xmin,rim_rim_xmax,rim_l0_rmax,car_tyre_rmin,car_tyre_rmax,car_tyre_w,car_l0_rmax,rim_l0fix_rmax,rim_l0fix_w,morph,rim_l1_rmax,rim_l1_w");
    let rows: Vec<Vec<String>> = st
        .query_map([], |r| {
            Ok((0..13)
                .map(|i| match r.get_ref(i).unwrap() {
                    rusqlite::types::ValueRef::Integer(x) => x.to_string(),
                    rusqlite::types::ValueRef::Real(x) => format!("{x:.4}"),
                    rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                    _ => String::new(),
                })
                .collect())
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let tyre = |s: &carbin::Subsection| s.name.to_lowercase().contains("tire");
    let rim = |s: &carbin::Subsection| !s.name.to_lowercase().contains("tire") && !s.name.to_lowercase().contains("blur");
    let f = |d: Option<Dims>| match d {
        Some(d) => [d.r_min, d.r_max, d.x_max - d.x_min, d.x_min, d.x_max],
        None => [f32::NAN; 5],
    };
    for row in rows {
        let (car, rimname) = (&row[0], &row[1]);
        let rc = read(&disc.join(format!("media/wheels/{rimname}.zip")), &format!("{rimname}.carbin"));
        let rs = rc.as_ref().and_then(|c| c.sections.first());
        let rt = f(rs.and_then(|s| dims(s, &tyre)));
        let rr = f(rs.and_then(|s| dims(s, &rim)));
        let rl0 = f(rs.and_then(|s| dims(s, &|x: &carbin::Subsection| x.lod == 0 && rim(x))));
        FIX.store(true, std::sync::atomic::Ordering::Relaxed);
        let rl0f = f(rs.and_then(|s| dims(s, &|x: &carbin::Subsection| x.lod == 0 && rim(x))));
        FIX.store(false, std::sync::atomic::Ordering::Relaxed);
        let rl1 = f(rs.and_then(|s| dims(s, &|x: &carbin::Subsection| x.lod >= 1 && rim(x))));
        let morph = rs.map(|s| s.lod_raw.extra_stride).unwrap_or(0);
        let cc = read(&disc.join(format!("media/cars/{car}.zip")), &format!("{car}.carbin"));
        let cs = cc.as_ref().and_then(|c| c.sections.iter().find(|s| s.name.eq_ignore_ascii_case("wheel")));
        let ct = f(cs.and_then(|s| dims(s, &tyre)));
        let c0 = read(&disc.join(format!("media/cars/{car}.zip")), &format!("{car}_lod0.carbin"));
        let c0s = c0.as_ref().and_then(|c| c.sections.iter().find(|s| s.name.eq_ignore_ascii_case("wheel")));
        let cl0 = f(c0s.and_then(|s| dims(s, &rim)));
        println!(
            "{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{},{:.4},{:.4}",
            row.join(","),
            rt[0], rt[1], rt[2], rr[1], rr[3], rr[4], rl0[1], ct[0], ct[1], ct[2], cl0[1], rl0f[1], rl0f[2], morph, rl1[1], rl1[2]
        );
    }
}
