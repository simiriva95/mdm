//! Genera l'icona .ico (topolino pixel, multi-risoluzione) e la embedda
//! nell'exe: così i collegamenti di Start Menu e desktop hanno l'icona giusta.

// stesso sprite di ui.rs (duplicato: build.rs non può importare dal crate)
const PET: [&str; 7] = [
    "..........pp..",
    "..........pp..",
    "....oooooooo..",
    "p..oooooooooo.",
    ".ppoooooookop.",
    "...oooooooooo.",
    "....oo....oo..",
];
const PET_W: usize = 14;

fn color(c: char) -> Option<[u8; 4]> {
    match c {
        'o' => Some([0x6e, 0x76, 0x9c, 255]), // corpo
        'p' => Some([0xe8, 0xa0, 0xa8, 255]), // orecchie/zampe rosa
        'k' => Some([0x2c, 0x31, 0x4a, 255]), // occhio
        _ => None,
    }
}

fn frame_rgba(size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let scale = ((size as usize) / 16).max(1);
    let off_x = (size as usize - PET_W * scale) / 2;
    let off_y = (size as usize - PET.len() * scale) / 2;
    for (ry, row) in PET.iter().enumerate() {
        for (rx, c) in row.chars().enumerate() {
            let Some(px) = color(c) else { continue };
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = off_x + rx * scale + dx;
                    let y = off_y + ry * scale + dy;
                    let i = (y * size as usize + x) * 4;
                    rgba[i..i + 4].copy_from_slice(&px);
                }
            }
        }
    }
    rgba
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("mdm.ico");
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 32, 48, 256] {
        let image = ico::IconImage::from_rgba_data(size, size, frame_rgba(size));
        dir.add_entry(ico::IconDirEntry::encode(&image).expect("encode ico"));
    }
    dir.write(std::fs::File::create(&out).expect("crea ico")).expect("scrivi ico");

    winresource::WindowsResource::new()
        .set_icon(out.to_str().unwrap())
        .compile()
        .expect("embed icona nell'exe");
}
