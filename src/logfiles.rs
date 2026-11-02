//! Shared physical/logical identity for JSONL logs and their compressed archives.
//! Readers stream in memory; decoded conversation bytes are never written to disk.
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub(crate) fn compressed(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "zst")
        && path
            .with_extension("")
            .extension()
            .is_some_and(|ext| ext == "jsonl")
}

pub(crate) fn logical_path(path: &Path) -> PathBuf {
    if compressed(path) {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

/// Return one logical file while an archive and its plain twin coexist. The archive
/// may still be incomplete during compression, so the plain twin always wins.
pub(crate) fn discovered(path: &Path) -> Option<PathBuf> {
    if compressed(path) {
        let plain = logical_path(path);
        (!plain.is_file()).then_some(plain)
    } else {
        path.extension()
            .is_some_and(|ext| ext == "jsonl")
            .then(|| path.to_path_buf())
    }
}

/// Resolve again at read time: compression may have completed since discovery.
pub(crate) fn resolve(logical: &Path) -> io::Result<(PathBuf, Metadata)> {
    let plain = logical_path(logical);
    match fs::symlink_metadata(&plain) {
        Ok(meta) if meta.is_file() => return Ok((plain, meta)),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log is not a regular file",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut archive = plain.as_os_str().to_os_string();
    archive.push(".zst");
    let archive = PathBuf::from(archive);
    let meta = fs::symlink_metadata(&archive)?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "archive is not a regular file",
        ));
    }
    Ok((archive, meta))
}

pub(crate) fn open(path: &Path) -> io::Result<Box<dyn BufRead + Send>> {
    open_at(path, 0)
}

pub(crate) fn open_at(path: &Path, offset: u64) -> io::Result<Box<dyn BufRead + Send>> {
    if compressed(path) {
        if offset != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "compressed logs cannot seek",
            ));
        }
        open_archive(path)
    } else {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(BufReader::new(file)))
    }
}

/// Archives in the reversible trash use numeric names, so callers can explicitly
/// retain the original encoding even when the physical filename has no suffix.
pub(crate) fn open_archive(path: &Path) -> io::Result<Box<dyn BufRead + Send>> {
    let decoder = ruzstd::decoding::StreamingDecoder::new(BufReader::new(File::open(path)?))
        .map_err(io::Error::other)?;
    Ok(Box::new(BufReader::new(ZstdFrames(Some(decoder)))))
}

// StreamingDecoder stops after one frame. Native archives may concatenate frames;
// retain the buffered source between decoders so migration never drops the tail.
struct ZstdFrames(
    Option<ruzstd::decoding::StreamingDecoder<BufReader<File>, ruzstd::decoding::FrameDecoder>>,
);

impl io::Read for ZstdFrames {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(decoder) = &mut self.0 else {
                return Ok(0);
            };
            let read = decoder.read(buffer)?;
            if read != 0 {
                return Ok(read);
            }
            let mut source = self.0.take().expect("decoder exists").into_inner();
            if source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            self.0 = Some(
                ruzstd::decoding::StreamingDecoder::new(source)
                    .map_err(|_| std::io::Error::other("压缩会话包含无效的后续数据帧"))?,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn discovery_prefers_plain_and_retains_identity_after_compression() {
        let root = std::env::temp_dir().join(format!("usage-logfiles-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let plain = root.join("rollout.jsonl");
        let archive = root.join("rollout.jsonl.zst");
        fs::write(&plain, b"{\"first\":1}\n").unwrap();
        fs::write(&archive, b"not finished").unwrap();
        assert_eq!(discovered(&plain), Some(plain.clone()));
        assert!(discovered(&archive).is_none());
        assert_eq!(resolve(&archive).unwrap().0, plain);
        let packed = ruzstd::encoding::compress_to_vec(
            b"{\"first\":1}\n".as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        fs::write(&archive, packed).unwrap();
        fs::remove_file(&plain).unwrap();
        assert_eq!(discovered(&archive), Some(plain.clone()));
        let (selected, _) = resolve(&plain).unwrap();
        assert_eq!(selected, archive);
        let mut text = String::new();
        open(&selected).unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "{\"first\":1}\n");
        assert!(discovered(&root.join("other.zst")).is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn renamed_archives_read_every_frame_and_report_invalid_tails() {
        let root = std::env::temp_dir().join(format!("usage-multiframe-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("0");
        let mut frames = ruzstd::encoding::compress_to_vec(
            b"header\n".as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        frames.extend(ruzstd::encoding::compress_to_vec(
            b"body\n".as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        ));
        fs::write(&path, &frames).unwrap();
        let mut decoded = String::new();
        open_archive(&path)
            .unwrap()
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, "header\nbody\n");
        frames.extend_from_slice(b"invalid tail");
        fs::write(&path, frames).unwrap();
        assert!(
            open_archive(&path)
                .unwrap()
                .read_to_string(&mut String::new())
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
