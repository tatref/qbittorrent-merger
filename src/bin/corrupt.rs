use std::io::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("Usage: {} <path> <offset> <size>", args[0]);
        std::process::exit(1);
    }

    let path = &args[1];
    let offset: u64 = args[2].parse().expect("offset");
    let size: usize = args[3].parse().expect("size");

    let mut f = std::fs::File::options().write(true).open(path)?;
    f.seek(std::io::SeekFrom::Start(offset))?;
    let _ = f.write(&vec![0; size])?;

    Ok(())
}
