//! Pack `vm:boot({files = …})` into a cpio archive the kernel unpacks
//! after the profile's initrd.
//!
//! An initrd is a sequence of cpio archives, each optionally
//! compressed, and the kernel unpacks them in order into one rootfs
//! with later entries overwriting earlier ones by path. The agent
//! overlay already rides on that (see [`super::agent_overlay`]); the
//! per-boot files ride on it too, as a third archive appended after
//! the overlay so a test's file wins over both the profile's initrd
//! and the agent's.
//!
//! The archive is plain SVR4 "newc" (`070701`) wrapped in a gzip
//! container, and the container is not optional. The kernel accepts an
//! *uncompressed* segment only at a four-byte-aligned offset within the
//! whole initrd (`init/initramfs.c`: `*buf == '0' && !(this_header & 3)`),
//! and the offset an appended segment lands at is whatever the preceding
//! compressed stream happened to consume — so a bare cpio is rejected
//! with "invalid magic at start of compressed archive" on most builds and
//! accepted on one in four. A compressed segment is detected by its magic
//! at any offset, so the wrapper makes the append unconditionally valid.
//!
//! The deflate stream inside is stored (uncompressed) blocks: the files a
//! test injects are small, the guest kernel has to inflate it either way,
//! and a real compressor would be a dependency bought for nothing.
//!
//! The kernel's unpacker does not create missing parent directories —
//! it opens each file at its full path — so every ancestor of every
//! file is emitted as a directory entry first. A directory that
//! already exists in an earlier archive is harmless: the kernel's
//! mkdir fails EEXIST and it moves on, re-applying the mode.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use super::InjectedFile;

/// Default mode for an injected regular file when the test gives none.
pub const DEFAULT_FILE_MODE: u32 = 0o644;
/// Mode for the directories synthesised above injected files.
const DIR_MODE: u32 = 0o755;

const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;

/// Build the archive, gzip-wrapped and ready to append to an initrd.
pub fn build_segment(files: &[InjectedFile]) -> Vec<u8> {
    gzip_stored(&build(files))
}

/// Wrap `data` in a gzip container whose deflate stream is stored
/// blocks. Deterministic: no timestamp, no name, no OS fingerprint that
/// varies between hosts.
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff];
    // Stored deflate blocks: a header byte carrying BFINAL and BTYPE=00,
    // then LEN and its ones complement, then the bytes verbatim. 65535
    // is the most one block can carry.
    let mut chunks = data.chunks(0xffff).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        out.push(if chunks.peek().is_none() { 0x01 } else { 0x00 });
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

/// Build the archive. Returns the complete cpio bytes, trailer
/// included. An empty file list yields an archive holding only the
/// trailer, which the kernel accepts and which unpacks to nothing.
///
/// Guest paths are taken relative to the rootfs whether or not they
/// carry a leading slash; `.` and `..` components are rejected by
/// [`validate`], which callers run first so the error reaches the test
/// with the offending path named.
pub fn build(files: &[InjectedFile]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut ino: u32 = 1;

    // Every ancestor directory, deepest last, each once.
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for f in files {
        let rel = relative(&f.guest_path);
        let mut acc = String::new();
        for comp in rel.split('/').filter(|c| !c.is_empty()) {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(comp);
            dirs.insert(acc.clone());
        }
        // The last component is the file itself, not a directory.
        dirs.remove(&acc);
    }
    // BTreeSet order is lexicographic, which places every parent before
    // its children ("a" < "a/b"), so a plain iteration is creation order.
    for d in &dirs {
        push_entry(&mut out, ino, S_IFDIR | DIR_MODE, d, &[]);
        ino += 1;
    }
    for f in files {
        let rel = relative(&f.guest_path);
        let mode = S_IFREG | (f.mode & 0o7777);
        push_entry(&mut out, ino, mode, &rel, &f.content);
        ino += 1;
    }
    push_entry(&mut out, 0, 0, "TRAILER!!!", &[]);
    out
}

/// Check every path is absolute-or-relative to the rootfs with no `.`
/// or `..` components and no empty name. Returns the first offender.
pub fn validate(files: &[InjectedFile]) -> Result<(), String> {
    for f in files {
        let p = &f.guest_path;
        let mut seen_normal = false;
        for c in p.components() {
            match c {
                Component::RootDir | Component::Prefix(_) => {}
                Component::Normal(_) => seen_normal = true,
                Component::CurDir | Component::ParentDir => {
                    return Err(format!(
                        "boot_opts.files: path `{}` may not contain `.` or `..`",
                        p.display()
                    ));
                }
            }
        }
        if !seen_normal {
            return Err(format!(
                "boot_opts.files: path `{}` names no file",
                p.display()
            ));
        }
    }
    Ok(())
}

fn relative(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.trim_start_matches('/').to_owned()
}

fn push_entry(out: &mut Vec<u8>, ino: u32, mode: u32, name: &str, data: &[u8]) {
    let namesize = name.len() as u32 + 1; // NUL included
    let filesize = data.len() as u32;
    out.extend_from_slice(b"070701");
    for field in [
        ino, mode, 0, 0, 1, 0, filesize, 0, 0, 0, 0, namesize, 0,
    ] {
        out.extend_from_slice(format!("{field:08x}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    pad4(out);
    out.extend_from_slice(data);
    pad4(out);
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(path: &str, content: &str, mode: u32) -> InjectedFile {
        InjectedFile {
            guest_path: PathBuf::from(path),
            content: content.as_bytes().to_vec(),
            mode,
        }
    }

    /// Parse the archive back into (name, mode, data) triples.
    fn entries(bytes: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let mut v = Vec::new();
        let mut pos = 0;
        loop {
            assert_eq!(&bytes[pos..pos + 6], b"070701", "magic at {pos}");
            let hex = |i: usize| {
                let s = std::str::from_utf8(&bytes[pos + 6 + i * 8..pos + 14 + i * 8]).unwrap();
                u32::from_str_radix(s, 16).unwrap()
            };
            let mode = hex(1);
            let filesize = hex(6) as usize;
            let namesize = hex(11) as usize;
            let name_start = pos + 110;
            let name =
                std::str::from_utf8(&bytes[name_start..name_start + namesize - 1]).unwrap();
            let mut data_start = name_start + namesize;
            data_start += (4 - data_start % 4) % 4;
            let data = bytes[data_start..data_start + filesize].to_vec();
            pos = data_start + filesize;
            pos += (4 - pos % 4) % 4;
            if name == "TRAILER!!!" {
                assert_eq!(pos, bytes.len(), "trailer is last");
                return v;
            }
            v.push((name.to_owned(), mode, data));
        }
    }

    #[test]
    fn parents_precede_files_and_modes_are_kept() {
        let files = [
            file("/system/prelude/hooks.seq.2", "hookseq 2\n", 0o644),
            file("usr/libexec/prelude/hooks.d/a.sh", "#!/usr/bin/sh\n", 0o755),
        ];
        let got = entries(&build(&files));
        let names: Vec<&str> = got.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "system",
                "system/prelude",
                "usr",
                "usr/libexec",
                "usr/libexec/prelude",
                "usr/libexec/prelude/hooks.d",
                "system/prelude/hooks.seq.2",
                "usr/libexec/prelude/hooks.d/a.sh",
            ]
        );
        assert_eq!(got[0].1, S_IFDIR | 0o755);
        assert_eq!(got[6].1, S_IFREG | 0o644);
        assert_eq!(got[7].1, S_IFREG | 0o755);
        assert_eq!(got[7].2, b"#!/usr/bin/sh\n");
    }

    #[test]
    fn empty_list_is_a_bare_trailer() {
        let bytes = build(&[]);
        assert!(entries(&bytes).is_empty());
        assert_eq!(bytes.len() % 4, 0);
    }

    #[test]
    fn root_level_file_has_no_directories() {
        let got = entries(&build(&[file("/hooks.seq", "hookseq 1\n", 0o644)]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "hooks.seq");
    }

    #[test]
    fn the_segment_is_a_gzip_container_the_kernel_will_recognise() {
        let seg = build_segment(&[file("/a", "hello", 0o644)]);
        assert_eq!(&seg[..3], &[0x1f, 0x8b, 0x08], "gzip magic and deflate method");
        // Round-trip through the system gunzip: stored blocks, correct
        // CRC and length, or this fails.
        let raw = build(&[file("/a", "hello", 0o644)]);
        let mut child = std::process::Command::new("gzip")
            .arg("-dc")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("gzip on PATH");
        {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(&seg).unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "gunzip: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.stdout, raw, "the container holds the archive unchanged");
    }

    #[test]
    fn an_empty_segment_still_inflates() {
        let seg = build_segment(&[]);
        let mut child = std::process::Command::new("gzip")
            .arg("-dc")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("gzip on PATH");
        {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(&seg).unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, build(&[]));
    }

    #[test]
    fn validate_rejects_dot_components_and_empty_paths() {
        assert!(validate(&[file("/a/../b", "", 0o644)]).is_err());
        assert!(validate(&[file("./a", "", 0o644)]).is_err());
        assert!(validate(&[file("/", "", 0o644)]).is_err());
        assert!(validate(&[file("/a/b", "", 0o644)]).is_ok());
    }
}
