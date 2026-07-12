//
// Merge identical files from different torrents via qBittorrent API
//

use itertools::Itertools;
use qbit_rs::Qbit;
use qbit_rs::model::{Credential, GetTorrentListArg, State};
use qbittorrent_merger::*;
use std::time::Duration;
use tokio::time::sleep;

use std::{collections::HashMap, fs::File};

use log::{debug, error, info, warn};

/// The ugly stuff
///
/// Overall process:
/// 1) find files with same size, then for each file:
/// 2) find missing pieces that belong to said file, then for each piece:
/// 3) get the file offset for the piece in the src torrent, and check if it is downloaded
/// 4) convert to piece in the dst torrent
/// 5) convert to file offset in the dst torrent
/// 6) check if sha matches
/// 7) Copy data from src to dst files
///
/// Careful with:
/// * Pieces can have different sizes between torrents
/// * Pieces can be misaligned if some files are present before the file that we want to restore (acting as padding). In that case, if he padding file is incomplete, it is not possible to restore the 1st piece of the 2nd file, because we can not check a hash overlapping unknown data
/// * 1 piece from dst can have multiple corresponding pieces in src, because it can span multiple pieces
/// * Last piece is probably not handled correctly
///
async fn merge_torrents(
    api: &Qbit,
    src_hash: &str,
    dst_hash: &str,
    dir_mappings: &HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    //let preferences = api.get_preferences().await.unwrap();

    info!("src_hash: {}", src_hash);
    info!("dst_hash: {}", dst_hash);

    let src_torrent: TorrentInfo = TorrentInfo::new(&api, src_hash).await?;
    let dst_torrent = TorrentInfo::new(&api, dst_hash).await?;

    let mut unavailable_pieces = 0;
    let mut data_outside_file_block = 0;
    let mut restored_pieces = 0;

    debug!(
        "src_torrent.piece_size={}",
        src_torrent.properties.piece_size.unwrap()
    );
    debug!("src content:");
    for f in &src_torrent.content {
        debug!("{:10} {}", f.size, &f.name);
    }
    debug!(
        "dst_torrent.piece_size={}",
        dst_torrent.properties.piece_size.unwrap()
    );
    debug!("dst content:");
    for f in &dst_torrent.content {
        debug!("{:10} {}", f.size, &f.name);
    }

    let same_files = find_same_size_files(&src_torrent, &dst_torrent);
    if same_files.len() == 0 {
        info!("No identical file found");
        return Ok(());
    }
    info!("same files: {:?}", &same_files);

    for same_file in &same_files {
        let dst_filename = &same_file.1[0];
        info!("Working on {}", dst_filename);

        let missing_pieces = get_missing_pieces(&dst_torrent, dst_filename);
        debug!(
            "{} missing_pieces: {:?}",
            missing_pieces.len(),
            &missing_pieces
        );

        'missing_pieces_loop: for &missing_piece_idx in &missing_pieces[0..] {
            let dst_piece = TorrentPiece {
                idx: missing_piece_idx,
                piece_size: dst_torrent.properties.piece_size.unwrap() as u64,
            };
            debug!("Working on missing piece: {:?}", dst_piece);

            let missing_hash = dst_torrent.pieces_hashes[dst_piece.idx];

            let (filename, dst_file_block) =
                piece_to_file_block(&dst_torrent, &Piece::TorrentPiece(dst_piece)).unwrap();
            debug!("filename: {}, fileblock: {:?}", &filename, &dst_file_block);

            // TODO: handle all combinations of files
            let src_filename = match convert_filename(&same_files, &filename) {
                Ok(x) => x,
                Err(_) => continue,
            };
            debug!("dst/src filenames: {} / {}", &filename, &src_filename);
            let src_pieces =
                file_block_to_pieces(&src_torrent, &src_filename, &dst_file_block).unwrap();
            debug!("src_pieces: {:?}", &src_pieces);

            for src_piece in &src_pieces {
                let src_piece_is_available = src_torrent.piece_is_downloaded(src_piece);
                if !src_piece_is_available {
                    debug!("Skipping unavailable piece: {:?}", src_piece);
                    unavailable_pieces += 1;
                    continue 'missing_pieces_loop;
                }
            }

            let mut src_f = get_read_file(&src_torrent, &src_filename, dir_mappings)
                .unwrap_or_else(|_| panic!("Can't open file {:?}", &src_filename));
            let virt_src_piece = TorrentPiece::merge(&src_pieces).unwrap();
            debug!("virt_src_piece: {:?}", virt_src_piece);
            let (_src_filename, virt_src_file_block) =
                piece_to_file_block(&src_torrent, &Piece::VirtualPiece(virt_src_piece)).unwrap();
            debug!("virt_src_file_block: {:?}", virt_src_file_block);

            if virt_src_file_block.contains(&dst_file_block) {
                // OK!
            } else {
                error!("Can't get data outside file block");
                error!("Can't get data outside file block");
                data_outside_file_block += 1;
                continue 'missing_pieces_loop;
            }

            let data = match read_piece(&mut src_f, virt_src_file_block) {
                Ok(x) => x,
                Err(_) => continue 'missing_pieces_loop,
            };
            let data_offset = (dst_file_block.offset - virt_src_file_block.offset) as usize; // is positive
            let data = &data[data_offset..(data_offset + dst_file_block.size as usize)];
            let computed_hash = get_sha1(data);

            if computed_hash == missing_hash {
                debug!("pieces hashes match!");
                debug!("Writing to {}", dst_filename);
                let mut dst_f = match get_write_file(&dst_torrent, dst_filename, dir_mappings) {
                    Ok(f) => f,
                    Err(_e) => continue,
                };

                write_piece(&mut dst_f, dst_file_block, data).expect("Unable to write file");
                restored_pieces += 1;
            } else {
                warn!("pieces hashes don't match");
            }
        }
    }

    info!("Retored pieces: {}", restored_pieces);
    info!("Unavailable pieces: {}", unavailable_pieces);
    info!("Data outside file block: {}", data_outside_file_block);

    Ok(())
}

async fn work(config: &Config, hashes: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let credential = Credential::new(&config.username, &config.password);
    let api = Qbit::new(config.host.as_str(), credential);

    let version = api.get_version().await?;
    info!("qBittorrent version: {}", version);
    info!("hashes: {:?}", hashes);

    api.stop_torrents([hashes[1].clone()]).await?;
    std::thread::sleep(Duration::from_secs(1));

    // Loop over all couple of hashes
    for hashes in hashes.iter().combinations(2) {
        // Loop over (src, dst), (dst, src)
        for (src_hash, dst_hash) in &[(hashes[0], hashes[1]), (hashes[1], hashes[0])] {
            match merge_torrents(&api, &src_hash, &dst_hash, &config.dir_mappings).await {
                Ok(()) => (),
                Err(e) => error!("{}", e),
            }
        }
    }

    api.recheck_torrents(hashes).await?;
    println!("Rechecking torrents...");
    sleep(Duration::from_secs(1)).await;

    loop {
        let torrent_list_arg = GetTorrentListArg::builder()
            .hashes(hashes.join("|"))
            .build();

        let torrent_states = api.get_torrent_list(torrent_list_arg).await.unwrap();
        let torrent_states: Vec<_> = torrent_states
            .iter()
            .filter_map(|t| t.state.clone())
            .collect();
        //dbg!(&torrent_states);

        if torrent_states
            .iter()
            .any(|state| state == &State::CheckingDL || state == &State::CheckingUP)
        {
            sleep(Duration::from_secs(1)).await;
            continue;
        } else {
            break;
        }
    }

    api.start_torrents(hashes).await?;

    Ok(())
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        println!("Usage: {} hash1 hash2 ...", args[0]);
        std::process::exit(1);
    }
    let config: Config = serde_json::from_reader(File::open("config.json").unwrap()).unwrap();
    let hashes = &args[1..];

    work(&config, hashes).await.unwrap();
}
