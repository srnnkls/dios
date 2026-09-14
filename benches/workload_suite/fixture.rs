use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};

use dios::bench::registration_policy_from_env;
use dios::{DirectIo, FileId, Get, PageId, Pool, ReaderCtx, ReadyResult};

use super::catalog::{FILE_COUNT, FILE_PAGES, FRAMES, GRANULE, Lane, POLLS_MAX, PageNumber};

pub(super) struct Fixture {
    pub(super) directory: PathBuf,
    checksums_full: Vec<u64>,
    checksums_projection: Vec<u64>,
}

impl Fixture {
    pub(super) fn create(directory: &Path) -> Result<Self, String> {
        fs::create_dir(directory).map_err(|error| format!("create fixture: {error}"))?;
        let pages = (FILE_COUNT * FILE_PAGES) as usize;
        let mut fixture = Self {
            directory: directory.to_owned(),
            checksums_full: Vec::with_capacity(pages),
            checksums_projection: Vec::with_capacity(pages),
        };
        for file in 0..FILE_COUNT {
            fixture.create_file(file)?;
        }
        let mut output = File::create(directory.join("output.bin")).map_err(display_error)?;
        for _ in 0..32 {
            output
                .write_all(&[0_u8; GRANULE as usize])
                .map_err(display_error)?;
        }
        output.sync_all().map_err(display_error)?;
        Ok(fixture)
    }

    fn create_file(&mut self, file_index: u32) -> Result<(), String> {
        let mut file = File::create(self.input_path(file_index)).map_err(display_error)?;
        let mut chunk = vec![0_u8; GRANULE as usize * 16].into_boxed_slice();
        for first in (0..FILE_PAGES).step_by(16) {
            for (offset, page_bytes) in chunk.chunks_exact_mut(GRANULE as usize).enumerate() {
                let number = file_index
                    + (first + u32::try_from(offset).map_err(display_error)?) * FILE_COUNT;
                fill_page(page_bytes, PageNumber(number));
                self.checksums_full.push(0);
                self.checksums_projection.push(0);
            }
            file.write_all(&chunk).map_err(display_error)?;
        }
        file.sync_all().map_err(display_error)?;
        if file_index + 1 == FILE_COUNT {
            for number in 0..FILE_COUNT * FILE_PAGES {
                let mut bytes = [0_u8; GRANULE as usize];
                fill_page(&mut bytes, PageNumber(number));
                self.checksums_full[number as usize] = fold_bytes(&bytes);
                self.checksums_projection[number as usize] = fold_bytes(&bytes[..64]);
            }
        }
        Ok(())
    }

    pub(super) fn input_path(&self, index: u32) -> PathBuf {
        self.directory.join(format!("segment-{index}.bin"))
    }

    pub(super) fn pool(&self, direct: DirectIo) -> Result<(Pool, [FileId; 8], FileId), String> {
        let pool = Pool::builder()
            .frame_count(FRAMES)
            .granule(GRANULE)
            .max_concurrent_readers(4)
            .peak_guards_per_reader(2)
            .max_inflight_reads(32)
            .miss_headroom(96)
            .max_retained_frames(16)
            .write_slots(16)
            .max_inflight_product_ops(17)
            .registered_file_capacity(9)
            .registration_posture(registration_policy_from_env()?)
            .build()
            .map_err(display_error)?;
        let mut files = Vec::with_capacity(FILE_COUNT as usize);
        for index in 0..FILE_COUNT {
            files.push(
                pool.open(&self.input_path(index), direct)
                    .map_err(display_error)?,
            );
        }
        let output = pool
            .open(&self.directory.join("output.bin"), direct)
            .map_err(display_error)?;
        let files = files
            .try_into()
            .map_err(|_| "eight input files required".to_owned())?;
        Ok((pool, files, output))
    }

    pub(super) fn expected(&self, lane: Lane, round: u32) -> u64 {
        let checksums = if lane.useful_bytes() == 64 {
            &self.checksums_projection
        } else {
            &self.checksums_full
        };
        (0..lane.operations())
            .fold(0_u64, |sum, index| {
                sum.wrapping_add(checksums[lane.page(round, index).0 as usize])
            })
            .wrapping_mul(u64::from(lane.decode_passes()))
    }

    pub(super) fn verify_output(&self) -> Result<(), String> {
        let file = OpenOptions::new()
            .read(true)
            .open(self.directory.join("output.bin"))
            .map_err(display_error)?;
        let mut bytes = [0_u8; GRANULE as usize];
        for number in 0..32 {
            file.read_exact_at(&mut bytes, u64::from(number * GRANULE))
                .map_err(display_error)?;
            let mut expected = [0_u8; GRANULE as usize];
            fill_page(&mut expected, PageNumber(number));
            if bytes != expected {
                return Err(format!("output page {number} differs"));
            }
        }
        Ok(())
    }
}

pub(super) fn page_id(files: &[FileId; 8], page: PageNumber) -> PageId {
    assert!(page.0 < FILE_COUNT * FILE_PAGES);
    PageId::new(files[(page.0 % FILE_COUNT) as usize], page.0 / FILE_COUNT)
}

pub(super) fn fill_page(bytes: &mut [u8], page: PageNumber) {
    assert_eq!(bytes.len(), GRANULE as usize);
    for (index, word) in bytes.chunks_exact_mut(8).enumerate() {
        word.copy_from_slice(
            &page
                .word(u32::try_from(index).expect("word index fits u32"))
                .to_le_bytes(),
        );
    }
}

#[inline(never)]
pub(super) fn fold_bytes(bytes: &[u8]) -> u64 {
    assert!(bytes.len().is_multiple_of(8));
    bytes.chunks_exact(8).fold(0_u64, |sum, word| {
        sum.wrapping_add(u64::from_le_bytes(
            word.try_into().expect("eight-byte word"),
        ))
    })
}

pub(super) fn prefill(pool: &Pool, files: &[FileId; 8], count: u32) -> Result<(), String> {
    let reader = pool.register_reader().map_err(display_error)?;
    // Prime lazy backend synchronization using a page outside every measured trace.
    let spare = PageNumber(FILE_COUNT * FILE_PAGES - 1);
    warm_page(pool, &reader, page_id(files, spare))?;
    for number in 0..count {
        warm_page(pool, &reader, page_id(files, PageNumber(number)))?;
    }
    pool.poll();
    Ok(())
}

fn warm_page(pool: &Pool, reader: &ReaderCtx, page: PageId) -> Result<(), String> {
    let mut token = match pool.get(reader, page).map_err(display_error)? {
        Get::Hit(_) => return Ok(()),
        Get::Pending(token) => token,
        Get::Busy => return Err("prefill unexpectedly Busy".to_owned()),
    };
    for _ in 0..POLLS_MAX {
        pool.poll();
        match pool.ready(reader, token) {
            ReadyResult::Ready(_) => return Ok(()),
            ReadyResult::NotYet(pending) => token = pending,
            ReadyResult::Err(error) => return Err(display_error(error)),
        }
    }
    Err("prefill exceeded poll bound".to_owned())
}

pub(super) fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
