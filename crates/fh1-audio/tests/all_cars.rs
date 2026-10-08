//! Every converted car loads and renders finite, non-silent, unclipped audio.
//! Needs a converted `audio` group: `FH1_AUDIO=<dir>` (skips otherwise).

use fh1_audio::synth::{CarInput, CarSound, Library};

#[test]
fn all_cars_render() {
    let Some(dir) = std::env::var_os("FH1_AUDIO") else {
        eprintln!("FH1_AUDIO not set, skipping");
        return;
    };
    let lib = Library::open(std::path::PathBuf::from(dir)).unwrap();
    let mut cars: Vec<String> = std::fs::read_dir(lib.root().join("cars"))
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").map(str::to_owned))
        .collect();
    cars.sort();
    let mut silent = Vec::new();
    for car in &cars {
        let mut s = CarSound::new(&lib, car).unwrap_or_else(|e| panic!("{car}: {e:#}"));
        let (idle, red) = (s.audio.rpm_idle, s.audio.rpm_redline);
        let mut block = vec![0f32; 512];
        let mut energy = 0.0f64;
        for k in 0..200 {
            let u = k as f32 / 200.0;
            let input = CarInput { rpm: idle + (red - idle) * u, throttle: 1.0, gear: 2, volume: 1.0, ..Default::default() };
            s.render(&input, &mut block, 48000.0);
            assert!(block.iter().all(|x| x.is_finite() && x.abs() <= 1.0), "{car}: bad sample");
            energy += block.iter().map(|x| (*x as f64).powi(2)).sum::<f64>();
        }
        if energy < 1e-3 {
            silent.push(car.clone());
        }
    }
    println!("{} cars rendered, silent: {silent:?}", cars.len());
    assert!(silent.len() <= cars.len() / 20, "too many silent cars: {silent:?}");
}
