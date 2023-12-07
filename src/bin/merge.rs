// http://www.bittorrent.org/beps/bep_0003.html
#![allow(unused_imports)]

use std::collections::HashSet;
use std::convert::TryInto;
use std::fs::OpenOptions;
use std::io::{prelude::*, BufReader, BufWriter};
use std::{collections::HashMap, fs::File};

use qbit_rs::model::{Preferences, TorrentContent, TorrentProperty};
use qbit_rs::{
    model::{Credential, GetTorrentListArg, PieceState},
    Qbit,
};
use sha1::{Digest, Sha1};

struct Torrent {
    properties: TorrentProperty,
    content: Vec<TorrentContent>,
    pieces_states: Vec<PieceState>,
    pieces_hashes: Vec<[u8; 20]>,
}

impl Torrent {
    async fn new(api: &Qbit, hash: &str) -> Result<Self, qbit_rs::Error> {
        let pieces_hashes: Vec<[u8; 20]> = api
            .get_torrent_pieces_hashes(hash)
            .await?
            .iter()
            .map(|s| hex::decode(s).unwrap().try_into().unwrap())
            .collect();
        let pieces_states = api.get_torrent_pieces_states(hash).await?;
        let properties = api.get_torrent_properties(hash).await?;
        let content = api.get_torrent_contents(hash, None).await?;

        let torrent = Torrent {
            properties,
            content,
            pieces_states,
            pieces_hashes,
        };
        Ok(torrent)
    }

    fn piece_is_downloaded(&self, piece: &Piece) -> bool {
        match self.pieces_states[piece.idx] {
            PieceState::Downloaded => true,
            _ => false,
        }
    }
}

fn get_sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let sha1: [u8; 20] = hasher.finalize().into();

    sha1
}

#[derive(Debug, Copy, Clone)]
struct FileBlock {
    offset: u64,
    size: u64,
}

#[derive(Debug, Copy, Clone)]
struct Piece {
    idx: usize,
    piece_size: u64,
}

fn piece_to_file_block(
    torrent: &Torrent,
    piece: &Piece,
) -> Result<(String, FileBlock), Box<dyn std::error::Error>> {
    let mut offset = piece.idx as u64 * piece.piece_size;
    for f in &torrent.content {
        if offset < f.size {
            // piece is inside file
            let file_block = FileBlock {
                offset,
                size: piece.piece_size,
            };
            return Ok((f.name.clone(), file_block));
        } else {
            // maybe in next file?
            offset -= f.size;
        }
    }

    Err("Piece outside of torrent".into())
}

fn file_block_to_piece(
    torrent: &Torrent,
    path: &str,
    file_block: &FileBlock,
) -> Result<Vec<Piece>, Box<dyn std::error::Error>> {
    let piece_size = torrent.properties.piece_size.unwrap() as u64;
    let mut offset = 0;
    for f in &torrent.content {
        if f.name == path {
            if file_block.offset > f.size {
                return Err(format!("Offset beyond file {} {}", file_block.offset, path).into());
            } else {
                // offset inside file
                offset += file_block.offset;
                let start_idx = (offset / piece_size) as usize;
                //let end_idx = ((offset + file_block.size) / piece_size) as usize + 1; // TODO: offset+1? offset+block_size-1?
                let end_idx = ((offset + file_block.size).div_ceil(piece_size)) as usize;

                let result: Vec<Piece> = (start_idx..end_idx)
                    .map(|idx| Piece { idx, piece_size })
                    .collect();

                return Ok(result);
            }
        } else {
            offset += f.size;
        }
    }

    Err(format!("File not found {:?}", path).into())
}

fn convert_filename(
    same_files: &[(Vec<String>, Vec<String>)],
    filename: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    for (list_a, list_b) in same_files {
        for name in list_a {
            if name == filename {
                return Ok(list_b[0].clone());
            }
        }
        for name in list_b {
            if name == filename {
                return Ok(list_a[0].clone());
            }
        }
    }

    Err(format!("File not found {:?}", filename).into())
}

fn get_read_file(
    preferences: &Preferences,
    torrent_property: &TorrentProperty,
    path: &str,
) -> std::io::Result<BufReader<File>> {
    let path = if torrent_property.pieces_num.unwrap() == torrent_property.pieces_have.unwrap() {
        format!("{}/{}", torrent_property.save_path.as_ref().unwrap(), path)
    } else {
        format!("{}/{}", preferences.temp_path.as_ref().unwrap(), path)
    };
    //let path = format!("{}/{}", torrent_property.save_path.as_ref().unwrap(), path);
    let f = OpenOptions::new().read(true).open(path)?;
    Ok(BufReader::new(f))
}

fn get_write_file(
    preferences: &Preferences,
    torrent_property: &TorrentProperty,
    path: &str,
) -> std::io::Result<BufWriter<File>> {
    let path = if torrent_property.pieces_num.unwrap() == torrent_property.pieces_have.unwrap() {
        format!("{}/{}", torrent_property.save_path.as_ref().unwrap(), path)
    } else {
        format!("{}/{}", preferences.temp_path.as_ref().unwrap(), path)
    };
    //let path = format!("{}/{}", torrent_property.save_path.as_ref().unwrap(), path);

    dbg!(&path);
    let f = OpenOptions::new().write(true).open(&path)?;
    Ok(BufWriter::new(f))
}

fn write_piece(
    f: &mut BufWriter<File>,
    file_block: FileBlock,
    data: &[u8],
) -> std::io::Result<usize> {
    f.seek(std::io::SeekFrom::Start(file_block.offset))?;
    f.write(data)
}

fn read_piece(f: &mut BufReader<File>, file_block: FileBlock) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0; file_block.size as usize];
    f.seek(std::io::SeekFrom::Start(file_block.offset))?;
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn find_same_size_files(t1: &Torrent, t2: &Torrent) -> Vec<(Vec<String>, Vec<String>)> {
    let mut t1_files: HashMap<u64, Vec<String>> = HashMap::new();
    for f in t1.content.iter() {
        let size = f.size;
        let name = f.name.clone();

        t1_files.entry(size).or_default().push(name);
    }
    let mut t2_files: HashMap<u64, Vec<String>> = HashMap::new();
    for f in t2.content.iter() {
        let size = f.size;
        let name = f.name.clone();

        t2_files.entry(size).or_default().push(name);
    }

    let t1_keys: HashSet<u64> = t1_files.keys().copied().collect();
    let t2_keys: HashSet<u64> = t2_files.keys().copied().collect();

    let mut common_files: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    for common in t1_keys.intersection(&t2_keys) {
        let a = t1_files.get(common).unwrap().clone();
        let b = t2_files.get(common).unwrap().clone();

        common_files.push((a, b));
    }

    common_files
}

fn get_missing_pieces(torrent: &Torrent, path: &str) -> Vec<usize> {
    let piece_size = torrent.properties.piece_size.unwrap() as u64;

    let offset = get_file_offset(&torrent.content, path).unwrap();

    let starting_idx = offset / piece_size;
    let file_size = torrent
        .content
        .iter()
        .find(|f| f.name == path)
        .expect("File not found")
        .size;
    let n_pieces = file_size / piece_size;

    let last_idx = starting_idx + n_pieces;

    let missing_pieces_idx: Vec<usize> = torrent
        .pieces_states
        .iter()
        .enumerate()
        .filter_map(|(idx, piece_state)| {
            if idx as u64 >= starting_idx
                && idx as u64 <= last_idx
                && piece_state != &PieceState::Downloaded
            {
                Some(idx)
            } else {
                None
            }
        })
        .collect();

    missing_pieces_idx
}

fn get_file_offset(
    torrent_content: &[TorrentContent],
    path: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut offset = 0;
    let mut found = false;
    for file in torrent_content.iter() {
        if file.name == path {
            found = true;
            break;
        }
        offset += file.size;
    }

    if !found {
        return Err(format!("File not found: {:?}", path).into());
    }

    Ok(offset)
}

async fn work() -> Result<(), Box<dyn std::error::Error>> {
    let credential = Credential::new("admin", "");
    let api = Qbit::new("http://localhost:8080", credential);

    let version = api.get_version().await?;
    dbg!(version);

    let preferences = api.get_preferences().await.unwrap();

    let src_hash = "9ddec20aec74729ddd100b3f60bfb9a87a5ee3f0";
    let dst_hash = "3617e650eadd9372c44c8b73b0b95381dd100192";

    let src_torrent = Torrent::new(&api, src_hash).await?;
    let dst_torrent = Torrent::new(&api, dst_hash).await?;

    println!("src content:");
    for f in &src_torrent.content {
        dbg!(f.size, &f.name);
    }
    println!("dst content:");
    for f in &dst_torrent.content {
        dbg!(f.size, &f.name);
    }

    let same_files = find_same_size_files(&src_torrent, &dst_torrent);
    dbg!(&same_files);

    let _src_filename = &same_files[0].0[0].clone(); // recomputed from torrent content later
    let dst_filename = &same_files[0].1[0].clone();

    let missing_pieces = get_missing_pieces(&dst_torrent, dst_filename);
    dbg!(&missing_pieces);
    dbg!(missing_pieces.len());

    assert_eq!(missing_pieces.len(), 4);

    for &missing_piece_idx in &missing_pieces[1..] {
        let dst_piece = Piece {
            idx: missing_piece_idx,
            piece_size: dst_torrent.properties.piece_size.unwrap() as u64,
        };
        dbg!("missing piece:", dst_piece);

        let missing_hash = dst_torrent.pieces_hashes[dst_piece.idx];

        let (filename, dst_file_block) = piece_to_file_block(&dst_torrent, &dst_piece).unwrap();
        dbg!(&filename, &dst_file_block);

        let src_filename = convert_filename(&same_files, &filename).unwrap();
        dbg!(&filename, &src_filename);
        let src_pieces = file_block_to_piece(&src_torrent, &src_filename, &dst_file_block).unwrap();
        dbg!(&src_pieces);

        for src_piece in &src_pieces {
            let src_piece_is_available = src_torrent.piece_is_downloaded(&src_piece);
            dbg!(src_piece_is_available);

            dbg!(&src_piece);

            let mut src_f = get_read_file(&preferences, &src_torrent.properties, &src_filename)
                .expect(&format!("Can't open file {:?}", &src_filename));
            let (_src_filename, src_file_block) =
                piece_to_file_block(&src_torrent, &src_piece).unwrap();

            dbg!(src_file_block);

            if dst_file_block.offset >= src_file_block.offset
                && dst_file_block.offset + dst_file_block.size
                    <= src_file_block.offset + src_file_block.size
            {
                // OK!
            } else {
                panic!("Can't get data outside file block");
            }

            let data = read_piece(&mut src_f, src_file_block).expect("Can't read piece");
            let data_offset = (dst_file_block.offset - src_file_block.offset) as usize; // is positive
            let data = &data[data_offset..(data_offset + dst_file_block.size as usize)];
            let computed_hash = get_sha1(data);

            if computed_hash == missing_hash {
                println!("hashes match!");
                println!("Writing to {}", dst_filename);
                let mut dst_f = get_write_file(&preferences, &dst_torrent.properties, dst_filename)
                    .expect(&format!("Can't open file {:?}", &dst_filename));
                //write_piece(&mut dst_f, dst_file_block, &data).expect("Unable to write file");
                println!("wrote to {}", dst_filename);
            } else {
                panic!("hashes don't match");
            }
        }

        break;
    }

    println!("Please recheck torrent");
    println!("Zee end");

    Ok(())
}

#[tokio::main]
async fn main() {
    work().await.unwrap();
}
