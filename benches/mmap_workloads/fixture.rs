use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use dios::{DirectIo, FileId, Get, PageId, Pool, ReaderCtx, ReadyResult, RegistrationPolicy};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::catalog::{Arm, Cache, GRANULE, Lane, PAGES, POLLS_MAX, PageNumber, word};
use super::os::Mapping;

pub(super) fn create(directory: &Path) -> io::Result<()> {
    fs::create_dir(directory)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("pages.bin"))?;
    let mut hash = Sha256::new();
    let mut chunk = vec![0_u8; 256 * GRANULE as usize];
    for start in (0..=PAGES).step_by(256) {
        let count = (PAGES + 1 - start).min(256) as usize;
        for (offset, page) in chunk
            .chunks_exact_mut(GRANULE as usize)
            .take(count)
            .enumerate()
        {
            let page_number = PageNumber(start + u32::try_from(offset).expect("chunk page"));
            for (offset, bytes) in page.chunks_exact_mut(8).enumerate() {
                bytes.copy_from_slice(
                    &word(page_number, u32::try_from(offset).expect("word")).to_le_bytes(),
                );
            }
        }
        let bytes = &chunk[..count * GRANULE as usize];
        file.write_all(bytes)?;
        hash.update(bytes);
    }
    file.sync_all()?;
    let identity = serde_json::json!({"schema": 1, "pages": PAGES, "granule": GRANULE,
        "spare_pages": 1, "sha256": format!("{:x}", hash.finalize())});
    let mut manifest = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("fixture.json"))?;
    serde_json::to_writer_pretty(&mut manifest, &identity)?;
    manifest.sync_all()?;
    File::open(directory)?.sync_all()
}

#[derive(Serialize)]
pub(super) struct CacheWitness {
    pub(super) hot_expected: u32,
    pub(super) hot_resident: u32,
    pub(super) cold_expected: u32,
    pub(super) cold_resident: u32,
    pub(super) hot_present_ptes: u32,
    pub(super) cold_present_ptes: u32,
    pub(super) all_resident: u32,
}

pub(super) fn prepare(
    mapping: &Mapping,
    lane: Lane,
    arm: Arm,
    seed: u32,
) -> io::Result<CacheWitness> {
    mapping.discard_cache()?;
    mapping.advice(Arm::MmapRandom)?;
    for number in 0..lane.warm_pages() {
        std::hint::black_box(mapping.page(PageNumber(number))[0]);
    }
    if lane.cache() == Cache::Minor {
        mapping.discard_ptes()?;
    }
    mapping.advice(arm)?;
    let resident = mapping.residency()?;
    let present = mapping.present_ptes()?;
    let mut visited = vec![false; PAGES as usize];
    let mut witness = CacheWitness {
        hot_expected: 0,
        hot_resident: 0,
        cold_expected: 0,
        cold_resident: 0,
        hot_present_ptes: 0,
        cold_present_ptes: 0,
        all_resident: u32::try_from(resident.iter().filter(|flag| **flag & 1 == 1).count())
            .expect("resident count"),
    };
    for index in 0..lane.operations() {
        let page = lane.page(seed, index).0;
        if visited[page as usize] {
            continue;
        }
        visited[page as usize] = true;
        let resident_count = u32::from(resident[page as usize] & 1 == 1);
        if page < lane.warm_pages() {
            witness.hot_expected += 1;
            witness.hot_resident += resident_count;
            witness.hot_present_ptes += u32::from(present[page as usize]);
        } else {
            witness.cold_expected += 1;
            witness.cold_resident += resident_count;
            witness.cold_present_ptes += u32::from(present[page as usize]);
        }
    }
    if witness.cold_resident != 0 {
        return Err(io::Error::other(
            "cold target remains resident after file-local advice",
        ));
    }
    if witness.hot_resident != witness.hot_expected {
        return Err(io::Error::other("warm target absent from page cache"));
    }
    if lane.cache() == Cache::Minor {
        if witness.hot_present_ptes != 0 {
            return Err(io::Error::other(
                "minor-fault targets still have present PTEs",
            ));
        }
    } else if witness.hot_present_ptes != witness.hot_expected {
        return Err(io::Error::other("warm targets lack present PTEs"));
    }
    assert_eq!(witness.cold_present_ptes, 0);
    Ok(witness)
}

pub(super) fn pool(path: &Path, lane: Lane, arm: Arm) -> Result<(Pool, FileId), String> {
    let mut builder = Pool::builder()
        .frame_count(lane.frames())
        .granule(GRANULE)
        .max_concurrent_readers(4)
        .peak_guards_per_reader(if lane.resident_access() { 2 } else { 1 })
        .max_retained_frames(if lane == Lane::ResidentAccessRetained {
            1024
        } else {
            0
        })
        .max_inflight_reads(64)
        .miss_headroom(192)
        .registered_file_capacity(1)
        .registration_posture(RegistrationPolicy::Unregistered);
    if arm != Arm::DiosAutomatic {
        builder = builder
            .readahead(dios::Readahead::Disabled)
            .prefetch_headroom(if arm.explicit_hints() { 16 } else { 0 });
    }
    let pool = builder.build().map_err(error)?;
    let file = pool.open(path, DirectIo::Required).map_err(error)?;
    let reader = pool.register_reader().map_err(error)?;
    warm_page(&pool, &reader, PageId::new(file, PAGES))?;
    for number in 0..lane.warm_pages() {
        warm_page(&pool, &reader, PageId::new(file, number))?;
    }
    pool.poll();
    drop(reader);
    Ok((pool, file))
}

fn warm_page(pool: &Pool, reader: &ReaderCtx, page: PageId) -> Result<(), String> {
    let mut pending = match pool.get(reader, page).map_err(error)? {
        Get::Hit(_) => return Ok(()),
        Get::Pending(token) => token,
        Get::Busy => return Err("prefill was Busy".to_owned()),
    };
    for _ in 0..POLLS_MAX {
        pool.poll();
        match pool.ready(reader, pending) {
            ReadyResult::Ready(_) => return Ok(()),
            ReadyResult::NotYet(token) => pending = token,
            ReadyResult::Err(failure) => return Err(error(failure)),
        }
    }
    Err("prefill exceeded fixed poll limit".to_owned())
}

pub(super) fn expected(lane: Lane, seed: u32) -> u64 {
    (0..lane.operations()).fold(0_u64, |sum, index| {
        let page = lane.page(seed, index);
        (0..lane.useful_bytes() / 8).fold(sum, |sum, offset| {
            sum.wrapping_add(word(page, offset + lane.word_offset(index)))
        })
    })
}

pub(super) fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}
