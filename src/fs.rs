//! 文件系统操作模块。
//!
//! 提供异步文件系统操作，基于 tokio 异步运行时。
//! `LocalFs` 的所有方法均为 async 关联函数，对 tokio::fs 做语义层面的薄封装。

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

// ==================== LocalFs ====================

/// 基于本地磁盘的异步文件系统操作。
pub struct FileSystem;

impl FileSystem {
    /// 读取文本文件的全部内容。
    pub async fn read_file(path: &Path) -> Result<String, FsError> {
        let metadata = tokio::fs::metadata(path).await;
        match metadata {
            Ok(meta) if !meta.is_file() => return Err(FsError::NotAFile(path.to_path_buf())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::NotFound(path.to_path_buf()));
            }
            Err(e) => return Err(FsError::Io(e)),
            _ => {}
        }
        tokio::fs::read_to_string(path).await.map_err(FsError::Io)
    }

    /// 将文本内容写入文件，如果父目录不存在会自动创建。
    pub async fn write_file(path: &Path, content: &str) -> Result<(), FsError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, content).await.map_err(FsError::Io)
    }

    /// 检查指定路径是否存在。
    pub async fn exists(path: &Path) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }

    /// 列出目录中的所有条目。
    pub async fn list_dir(path: &Path) -> Result<Vec<FileEntry>, FsError> {
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(path).await?;
        while let Some(entry) = dir.next_entry().await? {
            let meta = entry.metadata().await?;
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
                is_dir: meta.is_dir(),
                len: meta.len(),
            });
        }
        Ok(entries)
    }

    /// 递归创建目录，类似 `mkdir -p`。
    pub async fn create_dir_all(path: &Path) -> Result<(), FsError> {
        tokio::fs::create_dir_all(path).await.map_err(FsError::Io)
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
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn read_write_text_file() {
        let dir = TempDir::new();
        let file = dir.path().join("test.txt");

        FileSystem::write_file(&file, "hello world").await.unwrap();
        let content = FileSystem::read_file(&file).await.unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn write_overwrites_existing() {
        let dir = TempDir::new();
        let file = dir.path().join("overwrite.txt");

        FileSystem::write_file(&file, "first").await.unwrap();
        FileSystem::write_file(&file, "second").await.unwrap();
        assert_eq!(FileSystem::read_file(&file).await.unwrap(), "second");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = TempDir::new();
        let file = dir.path().join("a/b/c/deep.txt");

        FileSystem::write_file(&file, "nested").await.unwrap();
        assert_eq!(FileSystem::read_file(&file).await.unwrap(), "nested");
    }

    #[tokio::test]
    async fn read_nonexistent_file_returns_not_found() {
        let dir = TempDir::new();
        let file = dir.path().join("no_such.txt");

        let err = FileSystem::read_file(&file).await.unwrap_err();
        assert!(matches!(err, FsError::NotFound(p) if p == file));
    }

    #[tokio::test]
    async fn read_directory_returns_not_a_file() {
        let dir = TempDir::new();

        let err = FileSystem::read_file(dir.path()).await.unwrap_err();
        assert!(matches!(err, FsError::NotAFile(_)));
    }

    #[tokio::test]
    async fn exists_check() {
        let dir = TempDir::new();
        let file = dir.path().join("check.txt");

        assert!(!FileSystem::exists(&file).await);
        FileSystem::write_file(&file, "").await.unwrap();
        assert!(FileSystem::exists(&file).await);
    }

    #[tokio::test]
    async fn list_dir_returns_entries() {
        let dir = TempDir::new();

        FileSystem::write_file(&dir.path().join("a.txt"), "a").await.unwrap();
        FileSystem::write_file(&dir.path().join("b.txt"), "b").await.unwrap();
        FileSystem::create_dir_all(&dir.path().join("subdir")).await.unwrap();

        let entries = FileSystem::list_dir(dir.path()).await.unwrap();
        assert_eq!(entries.len(), 3);

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
        assert!(names.contains(&"subdir"));

        let subdir = entries.iter().find(|e| e.name == "subdir").unwrap();
        assert!(subdir.is_dir);
    }

    #[tokio::test]
    async fn create_dir_all_idempotent() {
        let dir = TempDir::new();
        let nested = dir.path().join("x/y/z");

        FileSystem::create_dir_all(&nested).await.unwrap();
        FileSystem::create_dir_all(&nested).await.unwrap();
        assert!(nested.is_dir());
    }
}
