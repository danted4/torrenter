fn main() {
    println!("cargo:rerun-if-changed=src/icon_raster.rs");
    #[cfg(windows)]
    embed_windows_icon();
}

#[cfg(windows)]
#[path = "src/icon_raster.rs"]
mod icon_raster;

#[cfg(windows)]
fn embed_windows_icon() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico_path = out.join("torrenter.ico");
    write_ico(&ico_path, &[16, 24, 32, 48, 64, 256]);
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().expect("utf-8 icon path"));
    res.compile().expect("embed torrenter.ico");
}

#[cfg(windows)]
fn write_ico(path: &std::path::Path, sizes: &[u32]) {
    let images: Vec<(u32, Vec<u8>)> = sizes
        .iter()
        .copied()
        .map(|size| (size, icon_raster::raster(size)))
        .collect();
    let count = images.len() as u16;
    let mut entries = Vec::new();
    let mut blobs = Vec::new();
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, rgba) in &images {
        let dib = rgba_to_dib(*size, rgba);
        entries.push(IcoEntry {
            size: *size,
            bytes: dib.len() as u32,
            offset,
        });
        offset += dib.len() as u32;
        blobs.push(dib);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    for entry in &entries {
        let dim = if entry.size >= 256 {
            0u8
        } else {
            entry.size as u8
        };
        out.push(dim);
        out.push(dim);
        out.push(0);
        out.push(0);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&entry.bytes.to_le_bytes());
        out.extend_from_slice(&entry.offset.to_le_bytes());
    }
    for blob in blobs {
        out.extend_from_slice(&blob);
    }
    std::fs::write(path, out).expect("write torrenter.ico");
}

#[cfg(windows)]
struct IcoEntry {
    size: u32,
    bytes: u32,
    offset: u32,
}

#[cfg(windows)]
fn rgba_to_dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let row_and = ((size + 31) / 32) * 4;
    let xor_size = size * size * 4;
    let and_size = row_and * size;
    let mut dib = Vec::with_capacity((40 + xor_size + and_size) as usize);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(size as i32).to_le_bytes());
    dib.extend_from_slice(&(size as i32 * 2).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&(xor_size + and_size).to_le_bytes());
    dib.extend_from_slice(&[0u8; 16]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            dib.push(rgba[i + 2]);
            dib.push(rgba[i + 1]);
            dib.push(rgba[i]);
            dib.push(rgba[i + 3]);
        }
    }
    dib.resize(dib.len() + and_size as usize, 0);
    dib
}
