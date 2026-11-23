//! 添付の実体（blob）の保存。
//!
//! - SHA-256 をキーに `<root>/<先頭2文字>/<sha256>` へ保存し、同じ中身は 1 つだけ持つ
//! - 受け取りながらハッシュを計算し、上限サイズを超えたら途中で打ち切る
//! - 一時ファイルに書いてから名前を変えるので、途中で落ちても壊れた blob は残らない
//! - blob はどの版からも参照される可能性があるため削除しない（`docs/spec.md` 3.1）

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{CoreError, Result};

/// 読み書きの単位
const CHUNK: usize = 64 * 1024;

/// SHA-256 の 16 進表記（小文字 64 文字）として正しいか。
///
/// パスに埋め込むので、これ以外の文字列は受け付けない（ディレクトリをまたぐ指定を防ぐ）。
pub fn is_valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// バイト列の SHA-256（小文字の 16 進）
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// blob の保存場所
#[derive(Clone, Debug)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// `root` を blob の保存先にする（なければ作る）
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// blob のパス（存在するとは限らない）
    pub fn path_for(&self, sha256: &str) -> Result<PathBuf> {
        if !is_valid_sha256(sha256) {
            return Err(CoreError::InvalidData(format!("不正な sha256: {sha256}")));
        }
        Ok(self.root.join(&sha256[..2]).join(sha256))
    }

    /// blob を持っているか
    pub fn contains(&self, sha256: &str) -> bool {
        self.path_for(sha256).map(|p| p.is_file()).unwrap_or(false)
    }

    /// ファイルを取り込む。上限を超えていれば読む前にエラーにする。
    pub fn import_file(&self, path: &Path, max_bytes: u64) -> Result<(String, u64)> {
        let size = fs::metadata(path)?.len();
        if size > max_bytes {
            return Err(CoreError::AttachmentTooLarge {
                size,
                max: max_bytes,
            });
        }
        self.import_reader(File::open(path)?, max_bytes)
    }

    /// 読み出し元から取り込み、`(sha256, サイズ)` を返す。同じ中身が既にあれば保存しない。
    pub fn import_reader(&self, reader: impl Read, max_bytes: u64) -> Result<(String, u64)> {
        let (tmp, sha, size) = self.write_temp(reader, max_bytes)?;
        self.commit_temp(&tmp, &sha)?;
        Ok((sha, size))
    }

    /// サーバーから受け取った blob を取り込む。ハッシュが `expected` と違えば保存しない。
    pub fn import_verified(
        &self,
        reader: impl Read,
        expected: &str,
        max_bytes: u64,
    ) -> Result<u64> {
        if !is_valid_sha256(expected) {
            return Err(CoreError::InvalidData(format!("不正な sha256: {expected}")));
        }
        let (tmp, sha, size) = self.write_temp(reader, max_bytes)?;
        if sha != expected {
            let _ = fs::remove_file(&tmp);
            return Err(CoreError::BlobHashMismatch {
                expected: expected.to_string(),
                actual: sha,
            });
        }
        self.commit_temp(&tmp, &sha)?;
        Ok(size)
    }

    /// 一時ファイルに書きながらハッシュとサイズを計算する
    fn write_temp(&self, mut reader: impl Read, max_bytes: u64) -> Result<(PathBuf, String, u64)> {
        let tmp = self
            .root
            .join("tmp")
            .join(format!("{}.part", Uuid::now_v7()));
        let result = (|| -> Result<(String, u64)> {
            let mut out = File::create(&tmp)?;
            let mut hasher = Sha256::new();
            let mut size: u64 = 0;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                };
                size += n as u64;
                if size > max_bytes {
                    return Err(CoreError::AttachmentTooLarge {
                        size,
                        max: max_bytes,
                    });
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n])?;
            }
            out.sync_all()?;
            Ok((hex::encode(hasher.finalize()), size))
        })();
        match result {
            Ok((sha, size)) => Ok((tmp, sha, size)),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// 一時ファイルを本来の場所へ移す。既にあれば一時ファイルを捨てる（重複排除）。
    fn commit_temp(&self, tmp: &Path, sha: &str) -> Result<()> {
        let dest = self.path_for(sha)?;
        if dest.is_file() {
            fs::remove_file(tmp)?;
            return Ok(());
        }
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::rename(tmp, &dest)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_same_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let (a, size) = store.import_reader(&b"hello"[..], 100).unwrap();
        let (b, _) = store.import_reader(&b"hello"[..], 100).unwrap();
        assert_eq!(a, b);
        assert_eq!(size, 5);
        assert_eq!(a, sha256_hex(b"hello"));
        assert!(store.contains(&a));
        // 実体は 1 つだけ、一時ファイルも残らない
        let files: Vec<_> = fs::read_dir(store.path_for(&a).unwrap().parent().unwrap())
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(
            fs::read_dir(dir.path().join("blobs/tmp")).unwrap().count(),
            0
        );
    }

    #[test]
    fn rejects_too_large_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let data = vec![7u8; 1000];
        let err = store.import_reader(&data[..], 999).unwrap_err();
        assert!(matches!(
            err,
            CoreError::AttachmentTooLarge { max: 999, .. }
        ));
        assert!(!store.contains(&sha256_hex(&data)));
        assert_eq!(
            fs::read_dir(dir.path().join("blobs/tmp")).unwrap().count(),
            0
        );

        // ファイルからの取り込みは読む前に判定する
        let f = dir.path().join("big.bin");
        fs::write(&f, &data).unwrap();
        assert!(matches!(
            store.import_file(&f, 10),
            Err(CoreError::AttachmentTooLarge {
                size: 1000,
                max: 10
            })
        ));
        // 上限ちょうどは受け付ける
        assert!(store.import_file(&f, 1000).is_ok());
    }

    #[test]
    fn verified_import_checks_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let wrong = sha256_hex(b"other");
        assert!(matches!(
            store.import_verified(&b"data"[..], &wrong, 100),
            Err(CoreError::BlobHashMismatch { .. })
        ));
        assert!(!store.contains(&wrong));
        let right = sha256_hex(b"data");
        assert_eq!(store.import_verified(&b"data"[..], &right, 100).unwrap(), 4);
        assert!(store.contains(&right));
    }

    #[test]
    fn rejects_invalid_sha_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        assert!(store.path_for("../etc/passwd").is_err());
        assert!(store.path_for(&"A".repeat(64)).is_err());
        assert!(!store.contains("zz"));
    }
}
