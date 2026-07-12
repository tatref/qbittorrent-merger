use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::convert::TryInto;
use std::fs::OpenOptions;
use std::io::{BufReader, BufWriter, prelude::*};
use std::path::PathBuf;
use std::{collections::HashMap, fs::File};

use log::info;
use qbit_rs::model::{GetTorrentListArg, Torrent, TorrentContent, TorrentProperty};
use qbit_rs::{Qbit, model::PieceState};
use sha1::{Digest, Sha1};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub host: String,
    pub username: String,
    pub password: String,
    pub dir_mappings: HashMap<String, String>,
}

#[allow(dead_code)]
pub struct TorrentInfo {
    pub hash: String,
    pub properties: TorrentProperty,
    pub content: Vec<TorrentContent>,
    pub pieces_states: Vec<PieceState>,
    pub pieces_hashes: Vec<[u8; 20]>,
    pub torrent: Torrent,
}

impl TorrentInfo {
    pub async fn new(api: &Qbit, hash: &str) -> Result<Self, qbit_rs::Error> {
        let pieces_hashes: Vec<[u8; 20]> = api
            .get_torrent_pieces_hashes(hash)
            .await?
            .iter()
            .map(|s| hex::decode(s).unwrap().try_into().unwrap())
            .collect();
        let pieces_states = api.get_torrent_pieces_states(hash).await?;
        let properties = api.get_torrent_properties(hash).await?;
        let content = api.get_torrent_contents(hash, None).await?;
        let torrent = api
            .get_torrent_list(
                GetTorrentListArg::builder()
                    .hashes(hash.to_string())
                    .build(),
            )
            .await?
            .first()
            .cloned()
            .unwrap();

        let torrent = TorrentInfo {
            hash: hash.to_owned(),
            properties,
            content,
            pieces_states,
            pieces_hashes,
            torrent,
        };
        Ok(torrent)
    }

    pub fn piece_is_downloaded(&self, piece: &TorrentPiece) -> bool {
        let piece = match self.pieces_states.get(piece.idx) {
            Some(p) => p,
            None => {
                // beyond last piece if alignment between src and dst is different
                // TODO: proper fix
                return false;
            }
        };
        match piece {
            PieceState::Downloaded => true,
            _ => false,
        }
    }
}

pub fn get_sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let sha1: [u8; 20] = hasher.finalize().into();

    sha1
}

/// A chunk of a file
#[derive(Debug, Copy, Clone)]
pub struct FileBlock {
    pub offset: u64,
    pub size: u64,
}
impl FileBlock {
    pub fn contains(&self, other: &Self) -> bool {
        self.offset <= other.offset && self.offset + self.size >= other.offset + other.size
    }
}

#[derive(Debug, Copy, Clone)]
pub enum Piece {
    /// Fake piece, not aligned
    VirtualPiece(VirtualPiece),
    /// A real piece from a torrent, starting offset is aligned on `piece_size`
    TorrentPiece(TorrentPiece),
}

#[derive(Debug, Copy, Clone)]
pub struct VirtualPiece {
    pub offset: usize,
    pub piece_size: u64,
}

#[derive(Debug, Copy, Clone)]
pub struct TorrentPiece {
    pub idx: usize,
    pub piece_size: u64,
}
impl TorrentPiece {
    /// Merge multiple consecutive pieces into one big virtual piece
    pub fn merge(list: &[TorrentPiece]) -> Option<VirtualPiece> {
        let first_piece = list.get(0)?;

        Some(VirtualPiece {
            offset: first_piece.idx * first_piece.piece_size as usize,
            piece_size: list.len() as u64 * first_piece.piece_size,
        })
    }
}

pub fn piece_to_file_block(
    torrent: &TorrentInfo,
    piece: &Piece,
) -> Result<(String, FileBlock), Box<dyn std::error::Error>> {
    match *piece {
        Piece::TorrentPiece(piece) => {
            let mut offset = piece.idx as u64 * piece.piece_size;
            for f in &torrent.content {
                if offset < f.size {
                    //let file_start = piece.idx as u64 * piece.piece_size - offset;
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
        Piece::VirtualPiece(piece) => {
            let mut offset = piece.offset as u64;
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
    }
}

pub fn file_block_to_pieces(
    torrent: &TorrentInfo,
    path: &str,
    file_block: &FileBlock,
) -> Result<Vec<TorrentPiece>, Box<dyn std::error::Error>> {
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
                let end_idx = ((offset + file_block.size).div_ceil(piece_size)) as usize;

                let result: Vec<TorrentPiece> = (start_idx..end_idx)
                    .map(|idx| TorrentPiece { idx, piece_size })
                    .collect();

                return Ok(result);
            }
        } else {
            offset += f.size;
        }
    }

    Err(format!("File not found {:?}", path).into())
}

pub fn convert_filename(
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

pub fn get_file_path(
    torrent_info: &TorrentInfo,
    filepath: &str,
    dir_mappings: &HashMap<String, String>,
) -> std::io::Result<PathBuf> {
    let root = torrent_info
        .torrent
        .content_path
        .clone()
        .ok_or(std::io::ErrorKind::Other)?;

    let mut new_root = root.clone();
    for (from, to) in dir_mappings {
        let re = Regex::new(from).unwrap();
        new_root = re.replace(&new_root, to).to_string();
    }
    if root != new_root {
        info!("file with mapping: {} -> {}", root, new_root);
    }

    let path = PathBuf::from(new_root).parent().unwrap().join(filepath);

    Ok(path)
}

pub fn get_read_file(
    torrent_info: &TorrentInfo,
    filepath: &str,
    dir_mappings: &HashMap<String, String>,
) -> std::io::Result<BufReader<File>> {
    let path = get_file_path(torrent_info, filepath, dir_mappings)?;
    let f = OpenOptions::new().read(true).open(path)?;
    Ok(BufReader::new(f))
}

pub fn get_write_file(
    torrent_info: &TorrentInfo,
    filepath: &str,
    dir_mappings: &HashMap<String, String>,
) -> std::io::Result<BufWriter<File>> {
    let path = get_file_path(torrent_info, filepath, dir_mappings)?;
    let f = OpenOptions::new().write(true).open(path)?;
    Ok(BufWriter::new(f))
}

pub fn write_piece(
    f: &mut BufWriter<File>,
    file_block: FileBlock,
    data: &[u8],
) -> std::io::Result<()> {
    f.seek(std::io::SeekFrom::Start(file_block.offset))?;
    f.write_all(data)?;
    f.flush()
}

pub fn read_piece(f: &mut BufReader<File>, file_block: FileBlock) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0; file_block.size as usize];
    f.seek(std::io::SeekFrom::Start(file_block.offset))?;
    f.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn find_same_size_files(t1: &TorrentInfo, t2: &TorrentInfo) -> Vec<(Vec<String>, Vec<String>)> {
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

pub fn get_missing_pieces(torrent: &TorrentInfo, path: &str) -> Vec<usize> {
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

pub fn get_file_offset(
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
