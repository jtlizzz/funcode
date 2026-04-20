//! 文件系统操作模块。
//!
//! 提供文件系统抽象，支持文本文件的读写操作。
//! 默认实现基于本地文件系统（`LocalFs`），可通过 `FileSystem` trait 扩展为其他后端。

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

// ==================== 错误类型 ====================

/// 文件系统操作错误。
#[derive(Debug, Error)]
pub enum FsError {
    /// 指定路径不存在。
    #[error("file not found: {0}")]
    NotFound(PathBuf),
    /// 指定路径不是一个文件。
    #[error("not a file: {0}")]
    NotAFile(PathBuf),
    /// IO 错误。
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

// ==================== 目录条目 ====================

/// 目录中的条目信息。
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// 文件或目录名。
    pub name: String,
    /// 完整路径。
    pub path: PathBuf,
    /// 是否为目录。
    pub is_dir: bool,
    /// 文件大小（字节）。
    pub len: u64,
}

// ==================== FileSystem trait ====================

/// 文件系统抽象特征。
///
/// 提供文本文件读写和目录浏览能力，所有实现必须是线程安全的。
pub trait FileSystem: Send + Sync {
    /// 读取文本文件的全部内容。
    fn read_file(&self, path: &Path) -> Result<String, FsError>;

    /// 将文本内容写入文件，如果父目录不存在会自动创建。
    fn write_file(&self, path: &Path, content: &str) -> Result<(), FsError>;

    /// 检查指定路径是否存在。
    fn exists(&self, path: &Path) -> bool;

    /// 列出目录中的所有条目。
    fn list_dir(&self, path: &Path) -> Result<Vec<FileEntry>, FsError>;

    /// 递归创建目录，类似 `mkdir -p`。
    fn create_dir_all(&self, path: &Path) -> Result<(), FsError>;
}

// ==================== 本地文件系统实现 ====================

/// 基于本地磁盘的文件系统实现。
pub struct LocalFs;

impl FileSystem for LocalFs {
    fn read_file(&self, path: &Path) -> Result<String, FsError> {
        let metadata = fs::metadata(path);
        match metadata {
            Ok(meta) if !meta.is_file() => return Err(FsError::NotAFile(path.to_path_buf())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::NotFound(path.to_path_buf()));
            }
            Err(e) => return Err(FsError::Io(e)),
            _ => {}
        }
        fs::read_to_string(path).map_err(FsError::Io)
    }

    fn write_file(&self, path: &Path, content: &str) -> Result<(), FsError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content).map_err(FsError::Io)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<FileEntry>, FsError> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
                is_dir: meta.is_dir(),
                len: meta.len(),
            });
        }
        Ok(entries)
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), FsError> {
        fs::create_dir_all(path).map_err(FsError::Io)
    }
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用临时目录，创建时自动建立，销毁时自动清理。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "funcode_test_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn read_write_text_file() {
        let fs = LocalFs;
        let dir = TempDir::new();
        let file = dir.path().join("test.txt");

        fs.write_file(&file, "hello world").unwrap();
        let content = fs.read_file(&file).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn write_overwrites_existing() {
        let fs = LocalFs;
        let dir = TempDir::new();
        let file = dir.path().join("overwrite.txt");

        fs.write_file(&file, "first").unwrap();
        fs.write_file(&file, "second").unwrap();
        assert_eq!(fs.read_file(&file).unwrap(), "second");
    }

    #[test]
    fn write_creates_parent_dirs() {
        let fs = LocalFs;
        let dir = TempDir::new();
        let file = dir.path().join("a/b/c/deep.txt");

        fs.write_file(&file, "nested").unwrap();
        assert_eq!(fs.read_file(&file).unwrap(), "nested");
    }

    #[test]
    fn read_nonexistent_file_returns_not_found() {
        let fs = LocalFs;
        let dir = TempDir::new();
        let file = dir.path().join("no_such.txt");

        let err = fs.read_file(&file).unwrap_err();
        assert!(matches!(err, FsError::NotFound(p) if p == file));
    }

    #[test]
    fn read_directory_returns_not_a_file() {
        let fs = LocalFs;
        let dir = TempDir::new();

        let err = fs.read_file(dir.path()).unwrap_err();
        assert!(matches!(err, FsError::NotAFile(_)));
    }

    #[test]
    fn exists_check() {
        let fs = LocalFs;
        let dir = TempDir::new();
        let file = dir.path().join("check.txt");

        assert!(!fs.exists(&file));
        fs.write_file(&file, "").unwrap();
        assert!(fs.exists(&file));
    }

    #[test]
    fn list_dir_returns_entries() {
        let fs = LocalFs;
        let dir = TempDir::new();

        fs.write_file(&dir.path().join("a.txt"), "a").unwrap();
        fs.write_file(&dir.path().join("b.txt"), "b").unwrap();
        fs.create_dir_all(&dir.path().join("subdir")).unwrap();

        let entries = fs.list_dir(dir.path()).unwrap();
        assert_eq!(entries.len(), 3);

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
        assert!(names.contains(&"subdir"));

        let subdir = entries.iter().find(|e| e.name == "subdir").unwrap();
        assert!(subdir.is_dir);
    }

    #[test]
    fn create_dir_all_idempotent() {
        let fs = LocalFs;
        let dir = TempDir::new();
        let nested = dir.path().join("x/y/z");

        fs.create_dir_all(&nested).unwrap();
        fs.create_dir_all(&nested).unwrap();
        assert!(nested.is_dir());
    }
}
