//! Byte-split 7z and ZIP archives: `name.7z.001`, `name.zip.001`, and bare
//! `name.001`.
//!
//! These are not multi-volume archives in the RAR sense. A splitter (7-Zip's
//! `-v`, HJSplit, `split(1)`) cut one ordinary archive into consecutive byte
//! ranges, so the archive is exactly the parts read back to back. The host
//! names the first part; the rest are found beside it and presented to the
//! 7z and ZIP readers as one seekable stream. Nothing is joined on disk: the
//! source preopen is read-only, and a joined copy of a media-sized archive
//! would double the scratch footprint for no benefit.
//!
//! The joined stream's leading bytes decide the format, because a bare
//! `name.001` says nothing about what it holds.
//!
//! PKWARE's own split ZIP (`name.z01`, `name.z02`, …, `name.zip`) is a
//! different thing: every part restarts its offsets at zero, so the parts do
//! not form one archive when concatenated and the zip crate refuses multi-disk
//! archives. That layout is reported as unsupported rather than misread.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use scryer_plugin_sdk::{ArchivePluginFormat, ArchivePluginProcessResponse};

use crate::{failed_message, failed_response, unsupported_response};

const SEVENZ_SIGNATURE: &[u8] = &[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];
const ZIP_LOCAL_FILE_SIGNATURE: &[u8] = b"PK\x03\x04";
const ZIP_EMPTY_ARCHIVE_SIGNATURE: &[u8] = b"PK\x05\x06";
/// The marker PKWARE split and spanned archives begin with.
const ZIP_SPANNED_SIGNATURE: &[u8] = b"PK\x07\x08";

/// Open the archive the host named as one seekable stream, joining its
/// siblings when it is the first part of a byte-split set.
///
/// Returns the stream and the format to read it as. For a plain archive that
/// is the host's `format`; for a split set it is what the joined stream's
/// signature says.
pub(crate) fn open_archive(
    archive_path: &Path,
    format: ArchivePluginFormat,
) -> Result<(VolumeReader, ArchivePluginFormat), Box<ArchivePluginProcessResponse>> {
    let open_code = open_error_code(format);
    let file_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    if is_pkware_split_zip(archive_path, file_name) {
        return Err(Box::new(unsupported_response(
            "split ZIP archives in the .z01/.zip layout are not supported; \
             only byte-split .zip.001 sets can be extracted",
        )));
    }

    let Some(first) = split_part_name(file_name) else {
        let volume = Volume::at(archive_path.to_path_buf()).map_err(|error| {
            Box::new(failed_response(open_code, "failed to open archive", error))
        })?;
        return Ok((VolumeReader::new(vec![volume]), format));
    };

    let paths = collect_split_parts(archive_path, &first)?;
    let mut volumes = Vec::with_capacity(paths.len());
    for path in paths {
        let volume = Volume::at(path).map_err(|error| {
            Box::new(failed_response(
                open_code,
                "failed to open split archive volume",
                error,
            ))
        })?;
        volumes.push(volume);
    }
    let mut reader = VolumeReader::new(volumes);
    let format = detect_split_format(&mut reader)
        .map_err(|error| {
            Box::new(failed_response(
                open_code,
                "failed to read split archive",
                error,
            ))
        })?
        .map_err(|message| Box::new(unsupported_response(message)))?;
    Ok((reader, format))
}

fn open_error_code(format: ArchivePluginFormat) -> &'static str {
    match format {
        ArchivePluginFormat::Zip => "open_zip",
        _ => "open_7z",
    }
}

/// `name.001` → (`name`, 1, 3). The set name is everything before the final
/// dot; the part number needs at least three digits, which is what every
/// splitter writes and what keeps `name.7z` or `episode.264` from matching.
struct SplitPartName<'a> {
    set_name: &'a str,
    number: u32,
    width: usize,
}

fn split_part_name(file_name: &str) -> Option<SplitPartName<'_>> {
    let (set_name, digits) = file_name.rsplit_once('.')?;
    if set_name.is_empty() || digits.len() < 3 || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(SplitPartName {
        set_name,
        number: digits.parse().ok()?,
        width: digits.len(),
    })
}

/// Every part of `first`'s set in the same directory, in order.
///
/// Parts belong to the set when their name up to the final dot equals the
/// named part's, ignoring ASCII case. The numbering has to run 1, 2, 3, …
/// without a gap: a missing middle part would silently shift every byte after
/// it, and a missing first part leaves nothing to read.
fn collect_split_parts(
    archive_path: &Path,
    first: &SplitPartName<'_>,
) -> Result<Vec<PathBuf>, Box<ArchivePluginProcessResponse>> {
    let directory = archive_path.parent().unwrap_or_else(|| Path::new("."));
    let entries = fs::read_dir(directory).map_err(|error| {
        Box::new(failed_response(
            "read_volumes",
            "failed to list split archive volumes",
            error,
        ))
    })?;

    let mut parts = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            Box::new(failed_response(
                "read_volumes",
                "failed to list split archive volumes",
                error,
            ))
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(part) = split_part_name(file_name) else {
            continue;
        };
        if !part.set_name.eq_ignore_ascii_case(first.set_name) {
            continue;
        }
        if parts.insert(part.number, path.clone()).is_some() {
            return Err(Box::new(failed_message(
                "duplicate_volume",
                &format!(
                    "split archive has more than one file for volume {}",
                    volume_name(first, part.number)
                ),
            )));
        }
    }

    let mut ordered = Vec::with_capacity(parts.len());
    for (expected, (number, path)) in (1_u32..).zip(parts) {
        if number != expected {
            return Err(Box::new(missing_volume(first, expected)));
        }
        ordered.push(path);
    }
    if ordered.is_empty() {
        return Err(Box::new(missing_volume(first, 1)));
    }
    Ok(ordered)
}

fn volume_name(first: &SplitPartName<'_>, number: u32) -> String {
    format!("'{}.{number:0width$}'", first.set_name, width = first.width)
}

fn missing_volume(first: &SplitPartName<'_>, number: u32) -> ArchivePluginProcessResponse {
    failed_message(
        "missing_volume",
        &format!(
            "split archive is missing volume {}",
            volume_name(first, number)
        ),
    )
}

/// `name.zip` with a `name.z01` beside it, or a `name.zNN` part itself.
fn is_pkware_split_zip(archive_path: &Path, file_name: &str) -> bool {
    let Some((stem, extension)) = file_name.rsplit_once('.') else {
        return false;
    };
    let extension = extension.to_ascii_lowercase();
    if extension.len() >= 3
        && extension.starts_with('z')
        && extension[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        return true;
    }
    if extension != "zip" {
        return false;
    }
    let directory = archive_path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    let first_part = format!("{stem}.z01");
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(&first_part))
    })
}

/// Which archive the joined stream holds, from its leading bytes, or why it
/// cannot be read.
fn detect_split_format(
    reader: &mut VolumeReader,
) -> io::Result<Result<ArchivePluginFormat, &'static str>> {
    let mut signature = [0_u8; 6];
    let mut filled = 0;
    while filled < signature.len() {
        let read = reader.read(&mut signature[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    reader.seek(SeekFrom::Start(0))?;
    let signature = &signature[..filled];

    Ok(if signature.starts_with(SEVENZ_SIGNATURE) {
        Ok(ArchivePluginFormat::SevenZip)
    } else if signature.starts_with(ZIP_LOCAL_FILE_SIGNATURE)
        || signature.starts_with(ZIP_EMPTY_ARCHIVE_SIGNATURE)
    {
        Ok(ArchivePluginFormat::Zip)
    } else if signature.starts_with(ZIP_SPANNED_SIGNATURE) {
        // A spanned ZIP renamed to numbered parts: like the .z01 layout, its
        // offsets restart per part and the parts cannot be joined.
        Err("spanned ZIP archives are not supported")
    } else {
        Err("split archive volumes do not hold a 7z or ZIP archive")
    })
}

struct Volume {
    path: PathBuf,
    /// Offset of this volume's first byte in the joined stream.
    start: u64,
    len: u64,
}

impl Volume {
    fn at(path: PathBuf) -> io::Result<Self> {
        let len = fs::metadata(&path)?.len();
        Ok(Self {
            path,
            start: 0,
            len,
        })
    }
}

/// Consecutive files read as one stream.
///
/// Only one volume is open at a time, so a set of hundreds of parts does not
/// hold hundreds of descriptors, and a sequential read never seeks: the open
/// file's cursor is tracked and only moved when the caller jumped.
pub(crate) struct VolumeReader {
    volumes: Vec<Volume>,
    len: u64,
    position: u64,
    /// The open volume's index, its file, and that file's cursor.
    open: Option<(usize, fs::File, u64)>,
}

impl VolumeReader {
    fn new(mut volumes: Vec<Volume>) -> Self {
        let mut start = 0_u64;
        for volume in &mut volumes {
            volume.start = start;
            start = start.saturating_add(volume.len);
        }
        Self {
            volumes,
            len: start,
            position: 0,
            open: None,
        }
    }
}

impl Read for VolumeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.position >= self.len {
            return Ok(0);
        }
        // Empty volumes end where they start, so they are never selected.
        let index = self
            .volumes
            .partition_point(|volume| volume.start + volume.len <= self.position);
        let volume = &self.volumes[index];
        let offset = self.position - volume.start;
        let wanted = (volume.len - offset).min(buf.len() as u64) as usize;

        if self.open.as_ref().is_none_or(|(open, _, _)| *open != index) {
            let file = fs::File::open(&volume.path)?;
            self.open = Some((index, file, 0));
        }
        let Some((_, file, cursor)) = self.open.as_mut() else {
            unreachable!("a volume was opened above");
        };
        if *cursor != offset {
            file.seek(SeekFrom::Start(offset))?;
            *cursor = offset;
        }
        let read = file.read(&mut buf[..wanted])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "archive volume '{}' is shorter than when it was opened",
                    volume
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_default()
                ),
            ));
        }
        *cursor += read as u64;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for VolumeReader {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let position = match target {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the archive",
            )
        })?;
        self.position = position;
        Ok(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) {
        fs::write(dir.join(name), bytes).expect("write split fixture");
    }

    fn joined(reader: &mut VolumeReader) -> Vec<u8> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("read joined stream");
        bytes
    }

    #[test]
    fn parts_read_back_to_back_and_seek_across_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut stream = SEVENZ_SIGNATURE.to_vec();
        stream.extend((0..=255_u8).cycle().take(1_000));
        write(dir.path(), "Example.Show.S01E01.7z.001", &stream[..300]);
        write(dir.path(), "Example.Show.S01E01.7z.002", &stream[300..700]);
        write(dir.path(), "Example.Show.S01E01.7z.003", &stream[700..]);

        let (mut reader, format) = open_archive(
            &dir.path().join("Example.Show.S01E01.7z.001"),
            ArchivePluginFormat::SevenZip,
        )
        .unwrap_or_else(|response| panic!("{:?}", response.message));

        assert_eq!(format, ArchivePluginFormat::SevenZip);
        assert_eq!(joined(&mut reader), stream);

        let mut window = [0_u8; 10];
        reader.seek(SeekFrom::Start(295)).unwrap();
        reader.read_exact(&mut window).unwrap();
        assert_eq!(window, stream[295..305]);
        reader.seek(SeekFrom::End(-5)).unwrap();
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, stream[stream.len() - 5..]);
        reader.seek(SeekFrom::Current(-700)).unwrap();
        reader.read_exact(&mut window).unwrap();
        assert_eq!(window, stream[stream.len() - 700..stream.len() - 690]);
        assert!(reader.seek(SeekFrom::Current(-10_000)).is_err());
    }

    #[test]
    fn a_bare_numbered_set_is_typed_by_its_signature() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Example.Show.S01E01.001", b"PK\x03\x04first");
        write(dir.path(), "Example.Show.S01E01.002", b"second");

        let (mut reader, format) = open_archive(
            &dir.path().join("Example.Show.S01E01.001"),
            ArchivePluginFormat::SevenZip,
        )
        .unwrap_or_else(|response| panic!("{:?}", response.message));

        assert_eq!(format, ArchivePluginFormat::Zip);
        assert_eq!(joined(&mut reader), b"PK\x03\x04firstsecond");
    }

    #[test]
    fn a_split_set_that_holds_neither_format_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Example.Show.S01E01.mkv.001",
            b"\x1aE\xdf\xa3 media",
        );

        let response = open_archive(
            &dir.path().join("Example.Show.S01E01.mkv.001"),
            ArchivePluginFormat::Zip,
        )
        .err()
        .expect("media split must not open as an archive");

        assert_eq!(
            response.status,
            scryer_plugin_sdk::ArchivePluginStatus::UnsupportedFormat
        );
    }

    #[test]
    fn only_parts_of_the_same_set_are_joined() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Example.Show.S01E01.7z.001", SEVENZ_SIGNATURE);
        write(dir.path(), "EXAMPLE.SHOW.S01E01.7Z.002", b"-same-set");
        write(dir.path(), "Example.Show.S01E02.7z.002", b"-other-set");
        write(
            dir.path(),
            "Example.Show.S01E01.zip.002",
            b"-other-extension",
        );
        write(dir.path(), "Example.Show.S01E01.7z", b"-unsplit-sibling");

        let (mut reader, _) = open_archive(
            &dir.path().join("Example.Show.S01E01.7z.001"),
            ArchivePluginFormat::SevenZip,
        )
        .unwrap_or_else(|response| panic!("{:?}", response.message));

        let mut expected = SEVENZ_SIGNATURE.to_vec();
        expected.extend_from_slice(b"-same-set");
        assert_eq!(joined(&mut reader), expected);
    }

    #[test]
    fn a_gap_in_the_numbering_names_the_missing_volume() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Example.Show.S01E01.7z.001", SEVENZ_SIGNATURE);
        write(dir.path(), "Example.Show.S01E01.7z.002", b"two");
        write(dir.path(), "Example.Show.S01E01.7z.004", b"four");

        let response = open_archive(
            &dir.path().join("Example.Show.S01E01.7z.001"),
            ArchivePluginFormat::SevenZip,
        )
        .err()
        .expect("a gap must be refused");

        assert_eq!(response.error_code.as_deref(), Some("missing_volume"));
        assert!(
            response
                .message
                .as_deref()
                .is_some_and(|message| message.contains("'Example.Show.S01E01.7z.003'")),
            "{:?}",
            response.message
        );
    }

    #[test]
    fn a_missing_first_volume_is_named() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Example.Show.S01E01.zip.002", b"two");
        write(dir.path(), "Example.Show.S01E01.zip.003", b"three");

        let response = open_archive(
            &dir.path().join("Example.Show.S01E01.zip.002"),
            ArchivePluginFormat::Zip,
        )
        .err()
        .expect("a set without its first part must be refused");

        assert_eq!(response.error_code.as_deref(), Some("missing_volume"));
        assert!(
            response
                .message
                .as_deref()
                .is_some_and(|message| message.contains("'Example.Show.S01E01.zip.001'")),
            "{:?}",
            response.message
        );
    }

    #[test]
    fn two_files_for_one_volume_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Example.Show.S01E01.7z.001", SEVENZ_SIGNATURE);
        write(dir.path(), "Example.Show.S01E01.7z.0001", SEVENZ_SIGNATURE);

        let response = open_archive(
            &dir.path().join("Example.Show.S01E01.7z.001"),
            ArchivePluginFormat::SevenZip,
        )
        .err()
        .expect("ambiguous volumes must be refused");

        assert_eq!(response.error_code.as_deref(), Some("duplicate_volume"));
    }

    #[test]
    fn pkware_split_zip_is_reported_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Example.Show.S01E01.z01", b"PK\x07\x08part");
        write(dir.path(), "Example.Show.S01E01.zip", b"PK\x05\x06tail");

        for named in ["Example.Show.S01E01.zip", "Example.Show.S01E01.z01"] {
            let response = open_archive(&dir.path().join(named), ArchivePluginFormat::Zip)
                .err()
                .expect("the .z01 layout must not be read");
            assert_eq!(
                response.status,
                scryer_plugin_sdk::ArchivePluginStatus::UnsupportedFormat,
                "{named}"
            );
        }
    }

    #[test]
    fn an_unsplit_archive_opens_as_itself() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Example.Show.S01E01.zip", b"PK\x03\x04whole");
        write(dir.path(), "Example.Show.S01E01.zip.002", b"unrelated");

        let (mut reader, format) = open_archive(
            &dir.path().join("Example.Show.S01E01.zip"),
            ArchivePluginFormat::Zip,
        )
        .unwrap_or_else(|response| panic!("{:?}", response.message));

        assert_eq!(format, ArchivePluginFormat::Zip);
        assert_eq!(joined(&mut reader), b"PK\x03\x04whole");
    }
}
