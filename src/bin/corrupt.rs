use std::io::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let path = &args[1];
    let offset: u64 = args[2].parse().expect("offset");
    let size: usize = args[3].parse().expect("size");

    let mut f = std::fs::File::options().write(true).open(path).unwrap();
    f.seek(std::io::SeekFrom::Start(offset)).unwrap();
    let _ = f.write(&vec![0; size]).unwrap();

    Ok(())
}
